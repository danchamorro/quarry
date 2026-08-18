use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, FontFamily, FontId, Layout, RichText, TextStyle};
use egui_extras::{Column, TableBuilder};
use quarry_core::{
    FilterExportJob, FilterExportOutcome, FilterExportProgress, FilterIndex, FilterJob,
    FilterMatch, FilterOperator, FilterPredicate, FilterProgress, FilterQuery, FilterReadJob,
    FilterReadOutcome, HeaderMode, IndexConfig, IndexJob, IndexProgress, OpenOptions, Row,
    SaveAsJob, SaveAsOutcome, SearchJob, SearchMatch, SearchOutcome, SearchPosition,
    SearchProgress, Session, StructuralIndex,
};

const BOOTSTRAP_ROWS: usize = 40;
const OVERSCAN_ROWS: usize = 2;
const ROW_HEIGHT: f32 = 17.0;
const HEADER_HEIGHT: f32 = 30.0;
const GRID_TITLE_HEIGHT: f32 = 36.0;
const SCROLLBAR_WIDTH: f32 = 18.0;
const MIN_THUMB_HEIGHT: f32 = 24.0;
const MAX_VISIBLE_COLUMNS: usize = 32;
const MAX_COPY_BYTES: usize = 64 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const PATH_INPUT_ID: &str = "quarry-path-input";
const JUMP_INPUT_ID: &str = "quarry-jump-input";
const FIND_INPUT_ID: &str = "quarry-find-input";
const COLUMN_INPUT_ID: &str = "quarry-column-input";
const COLUMN_POSITION_INPUT_ID: &str = "quarry-column-position-input";
const FILTER_COLUMN_INPUT_ID: &str = "quarry-filter-column-input";
const FILTER_VALUE_INPUT_ID: &str = "quarry-filter-value-input";

fn main() -> eframe::Result<()> {
    let started = Instant::now();
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("com.quarry.egui-prototype")
            .with_inner_size([1280.0, 780.0])
            .with_min_inner_size([860.0, 540.0]),
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "Quarry — Viewer Alpha",
        options,
        Box::new(move |creation| {
            configure_style(&creation.egui_ctx);
            Ok(Box::new(QuarryApp::new(initial_path, started)))
        }),
    )
}

struct QuarryApp {
    path_input: String,
    jump_input: String,
    find_input: String,
    column_input: String,
    column_position_input: String,
    columns_open: bool,
    filter_rules: Vec<FilterRuleDraft>,
    filters_open: bool,
    delimiter_mode: DelimiterMode,
    header_mode: HeaderMode,
    document: Option<Document>,
    notice: Option<String>,
    close_confirmation_open: bool,
    close_after_save: bool,
    started: Instant,
    logged_first_update: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilterRuleDraft {
    column_input: String,
    operator: FilterOperator,
    value_input: String,
}

impl Default for FilterRuleDraft {
    fn default() -> Self {
        Self {
            column_input: "1".into(),
            operator: FilterOperator::Contains,
            value_input: String::new(),
        }
    }
}

impl QuarryApp {
    fn new(initial_path: Option<PathBuf>, started: Instant) -> Self {
        let path_input = initial_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut app = Self {
            path_input,
            jump_input: "1".into(),
            find_input: String::new(),
            column_input: "1".into(),
            column_position_input: "1".into(),
            columns_open: false,
            filter_rules: vec![FilterRuleDraft::default()],
            filters_open: false,
            delimiter_mode: DelimiterMode::Auto,
            header_mode: HeaderMode::Auto,
            document: None,
            notice: None,
            close_confirmation_open: false,
            close_after_save: false,
            started,
            logged_first_update: false,
        };
        if let Some(path) = initial_path {
            app.open_path_and_report(path);
        }
        app
    }

    fn open_options(&self) -> OpenOptions {
        OpenOptions {
            delimiter: self.delimiter_mode.delimiter(),
            header_mode: self.header_mode,
            ..OpenOptions::default()
        }
    }

    fn open_typed_path(&mut self) {
        if self.path_input.trim().is_empty() {
            self.notice = Some("Enter a file path to open.".into());
            return;
        }
        self.open_path_and_report(PathBuf::from(self.path_input.trim()));
    }

    fn open_path(&mut self, path: PathBuf) -> Result<(), String> {
        if let Some(document) = self.document.as_mut() {
            if document.save_as_job.is_some() {
                return Err("Cancel the active Save As before opening another file.".into());
            }
            document.commit_header_edit();
            if document.is_dirty() {
                return Err("Discard or save your changes before opening another file.".into());
            }
        }
        if self
            .document
            .as_ref()
            .is_some_and(|document| document.export_job.is_some())
        {
            return Err(
                "Cancel the active export and wait for it to finish before opening another file."
                    .into(),
            );
        }
        self.replace_document(path)
    }

    fn replace_document(&mut self, path: PathBuf) -> Result<(), String> {
        self.replace_document_with_options(path, self.open_options())
    }

    fn replace_document_with_options(
        &mut self,
        path: PathBuf,
        options: OpenOptions,
    ) -> Result<(), String> {
        let mut document = Document::prepare(&path, options)?;
        document.start_indexing()?;
        if let Some(current) = self.document.as_mut() {
            current.shutdown();
        }
        self.path_input = path.to_string_lossy().into_owned();
        self.jump_input = "1".into();
        self.column_input = "1".into();
        self.column_position_input = "1".into();
        self.columns_open = false;
        self.filter_rules = vec![FilterRuleDraft::default()];
        self.filters_open = false;
        self.document = Some(document);
        Ok(())
    }

    fn open_path_and_report(&mut self, path: PathBuf) {
        let result = self.open_path(path);
        self.notice = result.err();
    }

    fn open_picker_result(&mut self, path: Option<PathBuf>) {
        if let Some(path) = path {
            self.open_path_and_report(path);
        }
    }

    fn choose_file(&mut self) {
        if let Some(document) = self.document.as_mut() {
            if document.save_as_job.is_some() {
                self.notice = Some("Cancel the active Save As before opening another file.".into());
                return;
            }
            document.commit_header_edit();
            if document.is_dirty() {
                self.notice =
                    Some("Discard or save your changes before opening another file.".into());
                return;
            }
        }
        let path = rfd::FileDialog::new()
            .set_title("Open a delimited file")
            .pick_file();
        self.open_picker_result(path);
    }

    fn choose_filtered_export(&mut self) {
        if let Some(document) = self.document.as_mut() {
            if document.save_as_job.is_some() {
                self.notice =
                    Some("Cancel the active Save As before exporting filtered rows.".into());
                return;
            }
            document.commit_header_edit();
            if document.is_dirty() {
                self.notice =
                    Some("Save or discard your changes before exporting filtered rows.".into());
                return;
            }
        }
        let Some(source) = self
            .document
            .as_ref()
            .map(|document| document.session.path().to_path_buf())
        else {
            self.notice = Some("Open and filter a file before exporting.".into());
            return;
        };
        let mut dialog = rfd::FileDialog::new()
            .set_title("Export filtered rows")
            .set_file_name(filtered_export_file_name(&source));
        if let Some(parent) = source.parent() {
            dialog = dialog.set_directory(parent);
        }
        self.export_picker_result(dialog.save_file());
    }

    fn choose_save_as(&mut self) -> bool {
        let Some(document) = self.document.as_mut() else {
            self.notice = Some("Open a file before using Save As.".into());
            return false;
        };
        document.commit_header_edit();
        if !document.is_save_as_ready() {
            self.notice = Some(if document.save_as_job.is_some() {
                "A Save As operation is already running.".into()
            } else if document.export_job.is_some() {
                "Cancel the active export before using Save As.".into()
            } else {
                "Rename a header before using Save As.".into()
            });
            return false;
        }
        let source = document.session.path().to_path_buf();
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save edited file as")
            .set_file_name(save_as_file_name(&source));
        if let Some(parent) = source.parent() {
            dialog = dialog.set_directory(parent);
        }
        self.save_as_picker_result(dialog.save_file())
    }

    fn save_as_picker_result(&mut self, destination: Option<PathBuf>) -> bool {
        let Some(destination) = destination else {
            return false;
        };
        let result = self
            .document
            .as_mut()
            .ok_or_else(|| "Open a file before using Save As.".to_owned())
            .and_then(|document| document.start_save_as(destination));
        self.notice = result.err();
        self.notice.is_none()
    }

    fn export_picker_result(&mut self, destination: Option<PathBuf>) {
        let Some(destination) = destination else {
            return;
        };
        let result = self
            .document
            .as_mut()
            .ok_or_else(|| "Open and filter a file before exporting.".to_owned())
            .and_then(|document| document.start_filtered_export(destination));
        self.notice = result.err();
    }

    fn reopen_document(&mut self) {
        let Some(path) = self
            .document
            .as_ref()
            .map(|document| document.session.path().to_path_buf())
        else {
            self.notice = Some("Open a file first.".into());
            return;
        };
        self.open_path_and_report(path);
    }

    fn handle_dropped_paths(&mut self, dropped: Vec<Option<PathBuf>>) {
        let count = dropped.len();
        let Some(path) = dropped.into_iter().flatten().next() else {
            self.notice = Some(format!(
                "Ignored {count} dropped item(s); Quarry can only open a local file."
            ));
            return;
        };
        let ignored = count.saturating_sub(1);
        let result = self.open_path(path);
        self.notice = match (result, ignored) {
            (Ok(()), 0) => None,
            (Ok(()), ignored) => Some(format!(
                "Opened one file and ignored {ignored} additional dropped item(s)."
            )),
            (Err(error), 0) => Some(error),
            (Err(error), ignored) => Some(format!(
                "{error} Ignored {ignored} additional dropped item(s)."
            )),
        };
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        if let Some(document) = self.document.as_mut() {
            document.commit_header_edit();
        }
        match action {
            Action::Open => return self.open_typed_path(),
            Action::Choose => return self.choose_file(),
            Action::Reopen => return self.reopen_document(),
            Action::ChooseSaveAs => {
                self.choose_save_as();
                return;
            }
            Action::ChooseFilteredExport => return self.choose_filtered_export(),
            Action::DiscardChanges => {
                if let Some(document) = self.document.as_mut() {
                    if document.save_as_job.is_some() {
                        self.notice =
                            Some("Wait for Save As to finish before discarding changes.".into());
                        return;
                    }
                    document.discard_header_edits();
                }
                self.notice = None;
                return;
            }
            Action::CopySelection => return self.copy_selection(ctx),
            Action::OpenColumns => {
                self.columns_open = true;
                return;
            }
            Action::OpenFilters => {
                self.filters_open = true;
                return;
            }
            _ => {}
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let result = match action {
            Action::Open
            | Action::Choose
            | Action::Reopen
            | Action::ChooseSaveAs
            | Action::ChooseFilteredExport
            | Action::DiscardChanges
            | Action::CopySelection
            | Action::OpenColumns
            | Action::OpenFilters => {
                unreachable!()
            }
            Action::PageUp => document.page(-1),
            Action::PageDown => document.page(1),
            Action::FirstColumns => {
                document.show_first_columns();
                Ok(())
            }
            Action::Jump => parse_data_row(&self.jump_input, document.data_start)
                .and_then(|start| document.navigate(start)),
            Action::FindNext => document.start_find_next(self.find_input.as_bytes()),
            Action::CancelSearch => {
                document.cancel_search();
                Ok(())
            }
            Action::ApplyFilter => {
                let predicates = self
                    .filter_rules
                    .iter()
                    .enumerate()
                    .map(|(index, rule)| {
                        parse_file_column(&rule.column_input, document.total_columns)
                            .map_err(|error| format!("Rule {}: {error}", index + 1))
                            .map(|column| FilterPredicate {
                                column,
                                operator: rule.operator,
                                value: rule.value_input.as_bytes().to_vec(),
                            })
                    })
                    .collect::<Result<Vec<_>, _>>();
                predicates.and_then(|predicates| document.start_filter(FilterQuery { predicates }))
            }
            Action::CancelFilter => {
                document.cancel_filter();
                Ok(())
            }
            Action::ClearFilter => document.clear_filter(),
            Action::CancelExport => {
                document.cancel_filtered_export();
                Ok(())
            }
            Action::CancelSaveAs => {
                document.cancel_save_as();
                Ok(())
            }
            Action::Cancel => {
                document.cancel();
                Ok(())
            }
        };
        self.notice = result.err();
    }

    fn apply_column_command(&mut self, command: ColumnCommand) {
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let result = match command {
            ColumnCommand::ViewInput => {
                parse_file_column(&self.column_input, document.total_columns)
                    .and_then(|column| document.view_column(column))
            }
            ColumnCommand::SetInputShown(shown) => {
                parse_file_column(&self.column_input, document.total_columns)
                    .and_then(|column| document.set_column_shown(column, shown))
            }
            ColumnCommand::MoveInput => {
                parse_file_column(&self.column_input, document.total_columns).and_then(|column| {
                    parse_column_position(&self.column_position_input, document.total_columns)
                        .and_then(|position| document.move_column(column, position))
                })
            }
            ColumnCommand::View(column) => document.view_column(column),
            ColumnCommand::SetShown { column, shown } => document.set_column_shown(column, shown),
            ColumnCommand::Move { column, position } => document.move_column(column, position),
            ColumnCommand::Reset => {
                document.reset_columns();
                Ok(())
            }
        };
        self.notice = result.err();
    }

    fn copy_selection(&mut self, ctx: &egui::Context) {
        let result = self
            .document
            .as_ref()
            .ok_or_else(|| "Open a file before copying.".to_owned())
            .and_then(Document::copy_selection_text);
        match result {
            Ok(text) => {
                ctx.copy_text(text);
                self.notice = None;
            }
            Err(error) => self.notice = Some(error),
        }
    }

    fn intercept_dirty_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        if let Some(document) = self.document.as_mut() {
            if document.save_as_job.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.close_confirmation_open = false;
                self.close_after_save = true;
                return;
            }
            document.commit_header_edit();
            if document.is_dirty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.close_confirmation_open = true;
            }
        }
    }

    fn keep_editing(&mut self) {
        self.close_confirmation_open = false;
        self.close_after_save = false;
    }
}

impl eframe::App for QuarryApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.logged_first_update {
            eprintln!(
                "quarry-egui first update: {:.3} ms",
                self.started.elapsed().as_secs_f64() * 1000.0
            );
            self.logged_first_update = true;
        }

        self.intercept_dirty_close(ctx);

        let dropped_paths = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>()
        });
        if !dropped_paths.is_empty() {
            self.handle_dropped_paths(dropped_paths);
        }

        if let Some(document) = &mut self.document
            && let Err(error) = document.poll()
        {
            self.notice = Some(error);
        }
        if let Some(document) = &mut self.document
            && let Err(error) = document.poll_search()
        {
            self.notice = Some(error);
        }
        if let Some(document) = &mut self.document
            && let Err(error) = document.poll_filter()
        {
            self.notice = Some(error);
        }
        if let Some(document) = &mut self.document
            && let Err(error) = document.poll_filtered_export()
        {
            self.notice = Some(error);
        }
        let save_as_result = self
            .document
            .as_mut()
            .map_or(Ok(None), Document::poll_save_as);
        match save_as_result {
            Ok(Some(destination)) => {
                let delimiter = self
                    .document
                    .as_ref()
                    .expect("saved document is still open")
                    .session
                    .dialect
                    .delimiter;
                let options = OpenOptions {
                    delimiter: Some(delimiter),
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                };
                match self.replace_document_with_options(destination.clone(), options) {
                    Ok(()) => {
                        self.notice = Some(format!("Saved as {}.", destination.display()));
                        if self.close_after_save {
                            self.close_after_save = false;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                    Err(error) => {
                        if let Some(document) = self.document.as_mut() {
                            document.save_as_status = None;
                        }
                        self.notice = Some(format!(
                            "Saved {} but could not open it: {error}",
                            destination.display()
                        ));
                        if self.close_after_save {
                            self.close_after_save = false;
                            self.close_confirmation_open = true;
                        }
                    }
                }
            }
            Ok(None) => {
                if self.close_after_save
                    && self
                        .document
                        .as_ref()
                        .is_some_and(|document| document.save_as_job.is_none())
                {
                    self.close_after_save = false;
                    self.close_confirmation_open = true;
                }
            }
            Err(error) => {
                self.notice = Some(error);
                if self.close_after_save {
                    self.close_after_save = false;
                    self.close_confirmation_open = true;
                }
            }
        }

        let mut action = None;
        egui::TopBottomPanel::top("quarry-toolbar")
            .frame(panel_frame(Color32::from_rgb(230, 235, 238)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("QUARRY")
                            .monospace()
                            .strong()
                            .size(21.0)
                            .color(Color32::from_rgb(24, 35, 42)),
                    );
                    ui.label(
                        RichText::new("VIEWER ALPHA")
                            .monospace()
                            .size(10.0)
                            .color(Color32::from_rgb(49, 85, 217)),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let state = if self.document.as_ref().is_some_and(Document::is_dirty) {
                            "MODIFIED"
                        } else {
                            "EDITABLE"
                        };
                        ui.label(RichText::new(state).monospace().size(10.0));
                    });
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let export_active = self.document.as_ref().is_some_and(|document| {
                        document.export_job.is_some() || document.save_as_job.is_some()
                    });
                    let dirty = self.document.as_ref().is_some_and(Document::is_dirty);
                    let label = ui.label("File");
                    let width = (ui.available_width() - 168.0).max(200.0);
                    let response = ui
                        .add_sized(
                            [width, 28.0],
                            egui::TextEdit::singleline(&mut self.path_input)
                                .id(egui::Id::new(PATH_INPUT_ID))
                                .hint_text("/path/to/file.csv"),
                        )
                        .labelled_by(label.id);
                    if ui
                        .add_enabled(!export_active && !dirty, egui::Button::new("Choose…"))
                        .on_disabled_hover_text(if dirty {
                            "Discard or save your changes before opening another file."
                        } else {
                            "Cancel the active export and wait for it to finish first."
                        })
                        .clicked()
                    {
                        action = Some(Action::Choose);
                    }
                    if ui
                        .add_enabled(!export_active && !dirty, egui::Button::new("Open"))
                        .on_disabled_hover_text(if dirty {
                            "Discard or save your changes before opening another file."
                        } else {
                            "Cancel the active export and wait for it to finish first."
                        })
                        .clicked()
                        || (!export_active
                            && !dirty
                            && response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    {
                        action = Some(Action::Open);
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let delimiter_label = ui.label("Delimiter");
                    let _ = egui::ComboBox::from_id_salt("quarry-delimiter-mode")
                        .selected_text(self.delimiter_mode.label())
                        .show_ui(ui, |ui| {
                            for mode in DelimiterMode::ALL {
                                ui.selectable_value(&mut self.delimiter_mode, mode, mode.label());
                            }
                        })
                        .response
                        .labelled_by(delimiter_label.id);

                    let header_label = ui.label("Header");
                    let _ = egui::ComboBox::from_id_salt("quarry-header-mode")
                        .selected_text(header_mode_label(self.header_mode))
                        .show_ui(ui, |ui| {
                            for mode in
                                [HeaderMode::Auto, HeaderMode::FirstRow, HeaderMode::NoHeader]
                            {
                                ui.selectable_value(
                                    &mut self.header_mode,
                                    mode,
                                    header_mode_label(mode),
                                );
                            }
                        })
                        .response
                        .labelled_by(header_label.id);

                    if ui
                        .add_enabled(
                            self.document.is_some()
                                && !self.document.as_ref().is_some_and(Document::is_dirty)
                                && self
                                    .document
                                    .as_ref()
                                    .is_none_or(|document| document.export_job.is_none()),
                            egui::Button::new("Apply / Reopen"),
                        )
                        .on_disabled_hover_text(
                            if self.document.as_ref().is_some_and(Document::is_dirty) {
                                "Discard or save your changes before reopening the file."
                            } else {
                                "Cancel the active export and wait for it to finish first."
                            },
                        )
                        .clicked()
                    {
                        action = Some(Action::Reopen);
                    }
                    if let Some(document) = self
                        .document
                        .as_ref()
                        .filter(|document| document.is_dirty())
                    {
                        if ui
                            .add_enabled(document.is_save_as_ready(), egui::Button::new("Save As…"))
                            .on_disabled_hover_text("Wait for the active file operation to finish.")
                            .clicked()
                        {
                            action = Some(Action::ChooseSaveAs);
                        }
                        if ui
                            .add_enabled(
                                document.save_as_job.is_none(),
                                egui::Button::new("Discard Changes"),
                            )
                            .on_disabled_hover_text("Wait for Save As to finish.")
                            .clicked()
                        {
                            action = Some(Action::DiscardChanges);
                        }
                    }
                });

                if let Some(document) = &self.document {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let progress = document.indexed_fraction();
                        let status = document.index_status();
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .desired_width((ui.available_width() - 104.0).max(160.0))
                                .text(format!("{status} · {:.1}%", progress * 100.0)),
                        );
                        if document.is_indexing() && ui.button("Cancel").clicked() {
                            action = Some(Action::Cancel);
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let filter_active = document.filter_active();
                        let label = ui.label("Data row");
                        let jump = ui
                            .add_enabled(
                                !filter_active,
                                egui::TextEdit::singleline(&mut self.jump_input)
                                    .id(egui::Id::new(JUMP_INPUT_ID))
                                    .horizontal_align(Align::RIGHT)
                                    .desired_width(120.0),
                            )
                            .labelled_by(label.id);
                        if ui
                            .add_enabled(!filter_active, egui::Button::new("Jump"))
                            .clicked()
                            || (!filter_active
                                && jump.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                        {
                            action = Some(Action::Jump);
                        }
                        if let Some(page_action) = page_controls(ui) {
                            action = Some(page_action);
                        }
                        if let Some(column_action) =
                            column_window_controls(ui, document.columns.start)
                        {
                            action = Some(column_action);
                        }
                        let filter_label = if filter_active {
                            "Filters active…"
                        } else {
                            "Filters…"
                        };
                        if ui.button(filter_label).clicked() {
                            action = Some(Action::OpenFilters);
                        }
                        if let Some(copy_action) = copy_control(ui, document.selection.is_some()) {
                            action = Some(copy_action);
                        }
                    });
                    ui.add_space(6.0);
                    if document.filter_active() {
                        if let Some(export_action) = filtered_export_controls(ui, document) {
                            action = Some(export_action);
                        }
                    } else {
                        let search_progress = document.search_progress();
                        let search_status = document.search_status.as_deref().or_else(|| {
                            (!document.is_search_ready())
                                .then_some("Search is available after indexing completes.")
                        });
                        if let Some(search_action) = search_controls(
                            ui,
                            &mut self.find_input,
                            document.is_search_ready(),
                            search_progress.as_ref(),
                            search_status,
                        ) {
                            action = Some(search_action);
                        }
                    }
                    if let Some(progress) = document.save_as_job.as_ref().map(SaveAsJob::progress) {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let fraction = if progress.total_bytes == 0 {
                                if progress.done { 1.0 } else { 0.0 }
                            } else {
                                (progress.bytes_scanned as f32 / progress.total_bytes as f32)
                                    .clamp(0.0, 1.0)
                            };
                            let status = if document.save_as_cancel_requested {
                                "Cancelling Save As"
                            } else if progress.done {
                                "Save As finished"
                            } else {
                                "Saving edited file"
                            };
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .desired_width(260.0)
                                    .text(format!("{status} · {:.1}%", fraction * 100.0)),
                            );
                            if document.save_as_job.is_some()
                                && ui
                                    .add_enabled(
                                        !document.save_as_cancel_requested,
                                        egui::Button::new("Cancel Save As"),
                                    )
                                    .clicked()
                            {
                                action = Some(Action::CancelSaveAs);
                            }
                        });
                    }
                    if let Some(status) = document.save_as_status.as_deref() {
                        let response = ui.label(status);
                        let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                            node.set_live(egui::accesskit::Live::Polite);
                        });
                    }
                }

                if let Some(notice) = &self.notice {
                    ui.add_space(6.0);
                    ui.colored_label(Color32::from_rgb(171, 65, 53), notice);
                }
            });

        egui::TopBottomPanel::bottom("quarry-status")
            .frame(panel_frame(Color32::from_rgb(230, 235, 238)))
            .show(ctx, |ui| {
                if let Some(document) = &self.document {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format_bytes(document.session.file_size));
                        ui.separator();
                        ui.label(format!("{} columns", document.total_columns));
                        ui.separator();
                        ui.label(format!(
                            "{} delimiter",
                            display_delimiter(document.session.dialect.delimiter)
                        ));
                        if document.is_dirty() {
                            ui.separator();
                            ui.colored_label(
                                Color32::from_rgb(171, 65, 53),
                                "Modified (not saved)",
                            );
                        }
                        ui.separator();
                        ui.label(if document.session.dialect.has_header {
                            "header row"
                        } else {
                            "no header"
                        });
                        ui.separator();
                        ui.label(format!(
                            "{} data rows indexed",
                            document.available_data_rows()
                        ));
                        if document.filter_active() {
                            ui.separator();
                            ui.label(format!(
                                "{} filter matches",
                                document.available_filter_rows()
                            ));
                        }
                        ui.separator();
                        ui.label(format!(
                            "first rows {:.3} ms",
                            document.session.metrics.first_rows.as_secs_f64() * 1000.0
                        ));
                        if let Some(read) = document.last_viewport_read {
                            ui.separator();
                            ui.label(format!("viewport {:.3} ms", read.as_secs_f64() * 1000.0));
                        }
                    });
                } else {
                    ui.label("No file open · pass a path or paste one above");
                }
            });

        if action.is_none() && self.document.is_some() {
            action = ctx.input(|input| {
                if input.key_pressed(egui::Key::PageDown) {
                    Some(Action::PageDown)
                } else if input.key_pressed(egui::Key::PageUp) {
                    Some(Action::PageUp)
                } else {
                    None
                }
            });
        }
        if let Some(action) = action {
            self.apply(ctx, action);
        }

        let column_command = self.document.as_ref().and_then(|document| {
            show_column_manager(
                ctx,
                &mut self.columns_open,
                &mut self.column_input,
                &mut self.column_position_input,
                document,
            )
        });
        if let Some(command) = column_command {
            self.apply_column_command(command);
        }

        let filter_action = self.document.as_ref().and_then(|document| {
            show_filter_manager(
                ctx,
                &mut self.filters_open,
                &mut self.filter_rules,
                document,
            )
        });
        if let Some(action) = filter_action {
            self.apply(ctx, action);
        }

        let mut grid_error = None;
        egui::CentralPanel::default()
            .frame(panel_frame(Color32::from_rgb(244, 247, 248)))
            .show(ctx, |ui| {
                if let Some(document) = self.document.as_mut() {
                    if let Err(error) = show_grid(ui, document) {
                        grid_error = Some(error);
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            ui.heading("Open a delimited file");
                            ui.label("Quarry reads the first viewport before indexing the rest.");
                        });
                    });
                }
            });
        let copy_event_targets_selection = self.document.as_ref().is_some_and(|document| {
            document.selection.is_some()
                && selection_copy_requested(
                    ctx,
                    self.filter_rules.len(),
                    document.header_edit.as_ref().map(|edit| edit.column),
                )
        });
        if copy_event_targets_selection {
            self.copy_selection(ctx);
        }
        if grid_error.is_some() {
            self.notice = grid_error;
        }

        if self.close_confirmation_open {
            let mut discard_and_close = false;
            let mut keep_editing = false;
            let mut save_and_close = false;
            let modal =
                egui::Modal::new(egui::Id::new("quarry-close-confirmation")).show(ctx, |ui| {
                    ui.heading("Unsaved changes");
                    ui.label("This file has unsaved header changes.");
                    ui.label("Save them to a new file, keep editing, or discard them.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Keep Editing").clicked() {
                            keep_editing = true;
                        }
                        if ui
                            .add_enabled(
                                self.document
                                    .as_ref()
                                    .is_some_and(Document::is_save_as_ready),
                                egui::Button::new("Save As and Close…"),
                            )
                            .clicked()
                        {
                            save_and_close = true;
                        }
                        if ui
                            .add_enabled(
                                self.document
                                    .as_ref()
                                    .is_none_or(|document| document.save_as_job.is_none()),
                                egui::Button::new("Discard Changes and Close"),
                            )
                            .clicked()
                        {
                            discard_and_close = true;
                        }
                    });
                });
            keep_editing |= modal.should_close();
            if keep_editing {
                self.keep_editing();
            } else if save_and_close {
                self.close_confirmation_open = false;
                self.close_after_save = true;
                if !self.choose_save_as() {
                    self.close_after_save = false;
                    self.close_confirmation_open = true;
                }
            } else if discard_and_close {
                if let Some(document) = self.document.as_mut() {
                    document.discard_header_edits();
                }
                self.close_confirmation_open = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if self.document.as_ref().is_some_and(|document| {
            document.job.is_some()
                || document.search_job.is_some()
                || document.filter_job.is_some()
                || document.filter_rows_loading()
                || document.export_job.is_some()
                || document.save_as_job.is_some()
        }) {
            ctx.request_repaint_after(POLL_INTERVAL);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DelimiterMode {
    Auto,
    Comma,
    Tab,
    Pipe,
    Semicolon,
}

impl DelimiterMode {
    const ALL: [Self; 5] = [
        Self::Auto,
        Self::Comma,
        Self::Tab,
        Self::Pipe,
        Self::Semicolon,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Comma => "Comma",
            Self::Tab => "Tab",
            Self::Pipe => "Pipe",
            Self::Semicolon => "Semicolon",
        }
    }

    fn delimiter(self) -> Option<u8> {
        match self {
            Self::Auto => None,
            Self::Comma => Some(b','),
            Self::Tab => Some(b'\t'),
            Self::Pipe => Some(b'|'),
            Self::Semicolon => Some(b';'),
        }
    }
}

fn header_mode_label(mode: HeaderMode) -> &'static str {
    match mode {
        HeaderMode::Auto => "Auto",
        HeaderMode::FirstRow => "First row is header",
        HeaderMode::NoHeader => "No header",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Open,
    Choose,
    Reopen,
    PageUp,
    PageDown,
    FirstColumns,
    OpenColumns,
    OpenFilters,
    ChooseSaveAs,
    CancelSaveAs,
    ChooseFilteredExport,
    DiscardChanges,
    Jump,
    FindNext,
    CancelSearch,
    ApplyFilter,
    CancelFilter,
    ClearFilter,
    CancelExport,
    CopySelection,
    Cancel,
}

fn page_controls(ui: &mut egui::Ui) -> Option<Action> {
    let page_up = ui.button("Page Up").clicked();
    let page_down = ui.button("Page Down").clicked();
    if page_up {
        Some(Action::PageUp)
    } else if page_down {
        Some(Action::PageDown)
    } else {
        None
    }
}

fn copy_control(ui: &mut egui::Ui, enabled: bool) -> Option<Action> {
    ui.add_enabled(enabled, egui::Button::new("Copy"))
        .on_hover_text("Copy the selected cell or row (⌘C)")
        .clicked()
        .then_some(Action::CopySelection)
}

fn filter_column_input_id(rule_index: usize) -> egui::Id {
    egui::Id::new((FILTER_COLUMN_INPUT_ID, rule_index))
}

fn filter_value_input_id(rule_index: usize) -> egui::Id {
    egui::Id::new((FILTER_VALUE_INPUT_ID, rule_index))
}

fn is_filter_text_input(focused: egui::Id, filter_rule_count: usize) -> bool {
    (0..filter_rule_count).any(|index| {
        focused == filter_column_input_id(index) || focused == filter_value_input_id(index)
    })
}

fn surrender_filter_text_focus(ctx: &egui::Context, filter_rule_count: usize) {
    let focused = ctx.memory(|memory| memory.focused());
    if let Some(focused) =
        focused.filter(|focused| is_filter_text_input(*focused, filter_rule_count))
    {
        ctx.memory_mut(|memory| memory.surrender_focus(focused));
    }
}

fn selection_copy_requested(
    ctx: &egui::Context,
    filter_rule_count: usize,
    edited_header: Option<usize>,
) -> bool {
    let copy_event = ctx.input(|input| {
        input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::Copy))
    });
    let text_input_focused = ctx.memory(|memory| {
        memory.focused().is_some_and(|focused| {
            [
                PATH_INPUT_ID,
                JUMP_INPUT_ID,
                FIND_INPUT_ID,
                COLUMN_INPUT_ID,
                COLUMN_POSITION_INPUT_ID,
            ]
            .into_iter()
            .any(|id| focused == egui::Id::new(id))
                || is_filter_text_input(focused, filter_rule_count)
                || edited_header.is_some_and(|column| focused == header_edit_id(column))
        })
    });
    copy_event && !text_input_focused
}

fn header_edit_id(column: usize) -> egui::Id {
    egui::Id::new(("quarry-header-edit", column))
}

fn column_window_controls(ui: &mut egui::Ui, column_start: usize) -> Option<Action> {
    let first = ui
        .add_enabled(column_start > 0, egui::Button::new("First columns"))
        .clicked();
    let manage = ui.button("Columns…").clicked();
    if first {
        Some(Action::FirstColumns)
    } else if manage {
        Some(Action::OpenColumns)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColumnCommand {
    ViewInput,
    SetInputShown(bool),
    MoveInput,
    View(usize),
    SetShown { column: usize, shown: bool },
    Move { column: usize, position: usize },
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ColumnDrag {
    column: usize,
}

fn show_column_manager(
    ctx: &egui::Context,
    open: &mut bool,
    column_input: &mut String,
    column_position_input: &mut String,
    document: &Document,
) -> Option<ColumnCommand> {
    let mut command = None;
    egui::Window::new("Columns")
        .id(egui::Id::new("quarry-column-manager"))
        .open(open)
        .default_width(560.0)
        .default_height(520.0)
        .min_width(440.0)
        .min_height(300.0)
        .resizable(true)
        .show(ctx, |ui| {
            ui.label("Choose which file columns appear and their left-to-right order.");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let label = ui.label("File column");
                let input = ui
                    .add_sized(
                        [96.0, 26.0],
                        egui::TextEdit::singleline(column_input)
                            .id(egui::Id::new(COLUMN_INPUT_ID))
                            .horizontal_align(Align::RIGHT),
                    )
                    .labelled_by(label.id);
                let can_view = document.total_columns > 0;
                if column_action_button(ui, "View", can_view, "View selected file column".into())
                    || (can_view
                        && input.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                {
                    command = Some(ColumnCommand::ViewInput);
                }
                if column_action_button(ui, "Hide", can_view, "Hide selected file column".into()) {
                    command = Some(ColumnCommand::SetInputShown(false));
                }
                if ui.button("Reset columns").clicked() {
                    command = Some(ColumnCommand::Reset);
                }
            });
            ui.horizontal(|ui| {
                let label = ui.label("Move to position");
                let position_input = ui
                    .add_sized(
                        [96.0, 26.0],
                        egui::TextEdit::singleline(column_position_input)
                            .id(egui::Id::new(COLUMN_POSITION_INPUT_ID))
                            .horizontal_align(Align::RIGHT),
                    )
                    .labelled_by(label.id);
                let can_move = document.total_columns > 0;
                if column_action_button(
                    ui,
                    "Move",
                    can_move,
                    "Move selected file column to display position".into(),
                ) || (can_move
                    && position_input.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                {
                    command = Some(ColumnCommand::MoveInput);
                }
            });
            ui.small("Positions include hidden columns. Drag a handle for nearby moves.");
            ui.label(format!(
                "{} shown · {} total · at most {} on screen",
                document.columns.shown_count(),
                document.total_columns,
                MAX_VISIBLE_COLUMNS
            ));
            ui.separator();
            if document.total_columns == 0 {
                ui.label("No columns");
                return;
            }
            ui.label("Display order. Top to bottom maps left to right.");
            let row_height = ui.spacing().interact_size.y;
            egui::ScrollArea::vertical()
                .id_salt("quarry-column-manager-list")
                .auto_shrink([false, false])
                .show_rows(ui, row_height, document.columns.order.len(), |ui, rows| {
                    for position in rows {
                        let column = document.columns.order[position];
                        ui.push_id(("managed-column", column), |ui| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                            let row = ui.horizontal(|ui| {
                                let handle = ui.add_sized(
                                    [48.0, row_height],
                                    egui::Button::new("Drag").sense(egui::Sense::drag()),
                                );
                                handle.dnd_set_drag_payload(ColumnDrag { column });
                                let _ = ui.ctx().accesskit_node_builder(handle.id, |node| {
                                    node.set_hidden();
                                });
                                handle.on_hover_text(format!(
                                    "Drag file column {} to reorder",
                                    column.saturating_add(1)
                                ));
                                let name = document.column_name(column);
                                let mut shown = !document.columns.hidden[column];
                                let checkbox_width = (ui.available_width() - 72.0).max(120.0);
                                if ui
                                    .add_sized(
                                        [checkbox_width, row_height],
                                        egui::Checkbox::new(
                                            &mut shown,
                                            format!("{}  {name}", column.saturating_add(1)),
                                        ),
                                    )
                                    .changed()
                                {
                                    command = Some(ColumnCommand::SetShown { column, shown });
                                }
                                if column_action_button(
                                    ui,
                                    "View",
                                    true,
                                    format!(
                                        "View file column {} ({name})",
                                        column.saturating_add(1)
                                    ),
                                ) {
                                    command = Some(ColumnCommand::View(column));
                                }
                            });
                            if ui.rect_contains_pointer(row.response.rect)
                                && let (Some(pointer), Some(dragged)) = (
                                    ui.input(|input| input.pointer.interact_pos()),
                                    egui::DragAndDrop::payload::<ColumnDrag>(ui.ctx()),
                                )
                            {
                                let (line_y, insertion) = if dragged.column == column {
                                    (row.response.rect.center().y, position)
                                } else if pointer.y < row.response.rect.center().y {
                                    (row.response.rect.top(), position)
                                } else {
                                    (row.response.rect.bottom(), position.saturating_add(1))
                                };
                                ui.painter().hline(
                                    row.response.rect.x_range(),
                                    line_y,
                                    egui::Stroke::new(2.0_f32, ui.visuals().selection.stroke.color),
                                );
                                if ui.input(|input| input.pointer.any_released())
                                    && let Some(dropped) =
                                        egui::DragAndDrop::take_payload::<ColumnDrag>(ui.ctx())
                                    && let Some(source_position) = document
                                        .columns
                                        .order
                                        .iter()
                                        .position(|source| *source == dropped.column)
                                {
                                    command = Some(ColumnCommand::Move {
                                        column: dropped.column,
                                        position: column_drop_position(
                                            source_position,
                                            insertion,
                                            document.columns.order.len(),
                                        ),
                                    });
                                }
                            }
                        });
                    }
                });
        });
    command
}

fn show_filter_manager(
    ctx: &egui::Context,
    open: &mut bool,
    rules: &mut Vec<FilterRuleDraft>,
    document: &Document,
) -> Option<Action> {
    let mut action = None;
    egui::Window::new("Filters")
        .id(egui::Id::new("quarry-filter-manager"))
        .open(open)
        .default_width(520.0)
        .min_width(420.0)
        .vscroll(true)
        .resizable(false)
        .show(ctx, |ui| {
            ui.label("Show only rows where all rules match (AND).");
            ui.add_space(6.0);
            let rule_count = rules.len();
            let sole_rule = rule_count == 1;
            let mut remove_index = None;
            for (index, rule) in rules.iter_mut().enumerate() {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!("Rule {}", index + 1));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let response =
                                ui.add_enabled(!sole_rule, egui::Button::new("Remove"));
                            let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                node.set_label(format!("Remove rule {}", index + 1));
                            });
                            if response.clicked() {
                                surrender_filter_text_focus(ui.ctx(), rule_count);
                                remove_index = Some(index);
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        let label = ui.label(format!("Rule {} file column (1-based)", index + 1));
                        let _ = ui
                            .add_sized(
                                [96.0, 26.0],
                                egui::TextEdit::singleline(&mut rule.column_input)
                                    .id(filter_column_input_id(index))
                                    .horizontal_align(Align::RIGHT),
                            )
                            .labelled_by(label.id);
                        if let Ok(column) =
                            parse_file_column(&rule.column_input, document.total_columns)
                        {
                            ui.label(document.column_name(column));
                        }
                    });
                    ui.horizontal(|ui| {
                        let label = ui.label(format!("Rule {} match", index + 1));
                        let _ = egui::ComboBox::from_id_salt((
                            "quarry-filter-operator",
                            index,
                        ))
                        .selected_text(filter_operator_label(rule.operator))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut rule.operator,
                                FilterOperator::Contains,
                                "Contains",
                            );
                            ui.selectable_value(
                                &mut rule.operator,
                                FilterOperator::Equals,
                                "Equals",
                            );
                        })
                        .response
                        .labelled_by(label.id);
                    });
                    let label = ui.label(format!("Rule {} value", index + 1));
                    let _ = ui
                        .add_sized(
                            [ui.available_width(), 48.0],
                            egui::TextEdit::multiline(&mut rule.value_input)
                                .desired_rows(2)
                                .id(filter_value_input_id(index))
                                .hint_text("Literal, case-sensitive text"),
                        )
                        .labelled_by(label.id);
                });
                ui.add_space(4.0);
            }
            if let Some(index) = remove_index {
                rules.remove(index);
            }
            if ui.button("Add AND rule").clicked() {
                surrender_filter_text_focus(ui.ctx(), rules.len());
                rules.push(FilterRuleDraft::default());
            }

            let can_apply = document.is_filter_ready()
                && document.search_job.is_none()
                && document.filter_job.is_none()
                && document.export_job.is_none()
                && !rules.is_empty()
                && rules.iter().all(|rule| {
                    parse_file_column(&rule.column_input, document.total_columns).is_ok()
                        && (rule.operator == FilterOperator::Equals || !rule.value_input.is_empty())
                });
            if ui
                .add_enabled(can_apply, egui::Button::new("Apply filters"))
                .clicked()
            {
                action = Some(Action::ApplyFilter);
            }
            ui.small("Contains requires a value. Equals can match an empty cell. Values are literal and case-sensitive.");

            if let Some(progress) = document.filter_progress() {
                ui.add_space(8.0);
                let fraction = if progress.file_size == 0 {
                    if progress.done { 1.0 } else { 0.0 }
                } else {
                    (progress.bytes_scanned as f32 / progress.file_size as f32).clamp(0.0, 1.0)
                };
                let text = if progress.cancelled && !progress.done {
                    format!(
                        "Cancelling · {:.1}% · {} matches",
                        fraction * 100.0,
                        progress.matches_found
                    )
                } else if !progress.done {
                    format!(
                        "Filtering · {:.1}% · {} matches",
                        fraction * 100.0,
                        progress.matches_found
                    )
                } else {
                    format!("{} matches", progress.matches_found)
                };
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(ui.available_width())
                        .text(text),
                );
            }

            if let Some(query) = document.filter_query.as_ref() {
                ui.add_space(6.0);
                ui.label(format!(
                    "Active: {} rule{} (all must match)",
                    query.predicates.len(),
                    if query.predicates.len() == 1 { "" } else { "s" }
                ));
                for (index, predicate) in query.predicates.iter().enumerate() {
                    let value = field_text(&predicate.value);
                    ui.label(format!(
                        "{}. file column {} ({}) {} {:?}",
                        index + 1,
                        predicate.column.saturating_add(1),
                        document.column_name(predicate.column),
                        filter_operator_label(predicate.operator).to_lowercase(),
                        value
                    ));
                }
            }
            if let Some(status) = document.filter_status.as_deref() {
                let response = ui.label(status);
                let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                    node.set_live(egui::accesskit::Live::Polite);
                });
            } else if document.search_job.is_some() {
                ui.label("Cancel the active search before filtering.");
            } else if !document.is_filter_ready() {
                ui.label("Open a file with at least one column to filter rows.");
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        document.filter_job.is_some()
                            && document
                                .filter_progress()
                                .is_some_and(|progress| !progress.done && !progress.cancelled),
                        egui::Button::new("Cancel filter"),
                    )
                    .clicked()
                {
                    action = Some(Action::CancelFilter);
                }
                if ui
                    .add_enabled(
                        document.filter_active() && document.export_job.is_none(),
                        egui::Button::new("Clear filter"),
                    )
                    .clicked()
                {
                    action = Some(Action::ClearFilter);
                }
            });
        });
    action
}

fn filter_operator_label(operator: FilterOperator) -> &'static str {
    match operator {
        FilterOperator::Contains => "Contains",
        FilterOperator::Equals => "Equals",
    }
}

fn column_drop_position(source_position: usize, insertion: usize, total_columns: usize) -> usize {
    insertion
        .saturating_sub(usize::from(source_position < insertion))
        .min(total_columns.saturating_sub(1))
}

fn column_action_button(
    ui: &mut egui::Ui,
    text: &str,
    enabled: bool,
    accessible_label: String,
) -> bool {
    let response = ui
        .add_enabled(enabled, egui::Button::new(text))
        .on_hover_text(&accessible_label);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            enabled && ui.is_enabled(),
            accessible_label.clone(),
        )
    });
    response.clicked()
}

fn search_controls(
    ui: &mut egui::Ui,
    query: &mut String,
    index_ready: bool,
    progress: Option<&SearchProgress>,
    status: Option<&str>,
) -> Option<Action> {
    let mut action = None;
    ui.horizontal(|ui| {
        let searching = progress.is_some();
        let label = ui.label("Find (literal, case-sensitive)");
        let input = ui
            .add_enabled(
                !searching,
                egui::TextEdit::singleline(query)
                    .id(egui::Id::new(FIND_INPUT_ID))
                    .hint_text("Text to find"),
            )
            .labelled_by(label.id);
        let can_find = index_ready && !searching && !query.is_empty();
        if ui
            .add_enabled(can_find, egui::Button::new("Find Next"))
            .clicked()
            || (can_find
                && input.lost_focus()
                && ui.input(|input| input.key_pressed(egui::Key::Enter)))
        {
            action = Some(Action::FindNext);
        }

        if let Some(progress) = progress {
            let fraction = if progress.total_bytes == 0 {
                if progress.done { 1.0 } else { 0.0 }
            } else {
                (progress.bytes_scanned as f32 / progress.total_bytes as f32).clamp(0.0, 1.0)
            };
            let text = if progress.cancelled {
                format!("Cancelling search · {:.1}%", fraction * 100.0)
            } else {
                format!("Searching · {:.1}%", fraction * 100.0)
            };
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(180.0)
                    .text(text),
            );
            if ui
                .add_enabled(!progress.cancelled, egui::Button::new("Cancel Search"))
                .clicked()
            {
                action = Some(Action::CancelSearch);
            }
        } else if let Some(status) = status {
            let response = ui.label(status);
            let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                node.set_live(egui::accesskit::Live::Polite);
            });
        }
    });
    action
}

fn filtered_export_controls(ui: &mut egui::Ui, document: &Document) -> Option<Action> {
    if !document.filter_active() {
        return None;
    }
    let mut action = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                document.is_filtered_export_ready() && !document.is_dirty(),
                egui::Button::new("Export Filtered Rows…"),
            )
            .on_hover_text(if document.is_dirty() {
                "Save or discard your changes before exporting filtered rows."
            } else {
                "Export all matching rows to a new file"
            })
            .clicked()
        {
            action = Some(Action::ChooseFilteredExport);
        }

        if let Some(progress) = document.filtered_export_progress() {
            let fraction = if progress.total_bytes == 0 {
                if progress.done { 1.0 } else { 0.0 }
            } else {
                (progress.bytes_scanned as f32 / progress.total_bytes as f32).clamp(0.0, 1.0)
            };
            let verb = if document.export_cancel_requested {
                "Cancelling export"
            } else if progress.cancelled {
                "Export cancelled"
            } else if document
                .export_status
                .as_deref()
                .is_some_and(|status| status.starts_with("Export failed"))
            {
                "Export failed"
            } else if progress.done {
                "Export finished"
            } else {
                "Exporting"
            };
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(220.0)
                    .text(format!(
                        "{verb} · {:.1}% · {} rows",
                        fraction * 100.0,
                        progress.rows_written
                    )),
            );
        }
        if document.export_job.is_some()
            && ui
                .add_enabled(
                    !document.export_cancel_requested,
                    egui::Button::new("Cancel Export"),
                )
                .clicked()
        {
            action = Some(Action::CancelExport);
        }
    });
    if let Some(status) = document.export_status.as_deref() {
        let response = ui.label(status);
        let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
            node.set_live(egui::accesskit::Live::Polite);
        });
    }
    action
}

fn filtered_export_file_name(source: &Path) -> String {
    let stem = source
        .file_stem()
        .or_else(|| source.file_name())
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "export".into());
    match source.extension() {
        Some(extension) => format!("{stem}-filtered.{}", extension.to_string_lossy()),
        None => format!("{stem}-filtered"),
    }
}

fn save_as_file_name(source: &Path) -> String {
    let stem = source
        .file_stem()
        .or_else(|| source.file_name())
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "edited".into());
    match source.extension() {
        Some(extension) => format!("{stem}-edited.{}", extension.to_string_lossy()),
        None => format!("{stem}-edited"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridSelection {
    Cell { row: u64, column: usize },
    Row { row: u64 },
}

impl GridSelection {
    fn row(self) -> u64 {
        match self {
            Self::Cell { row, .. } | Self::Row { row, .. } => row,
        }
    }

    fn selects_row(self, row: u64) -> bool {
        matches!(self, Self::Row { row: selected, .. } if selected == row)
    }

    fn selects_cell(self, row: u64, column: usize) -> bool {
        self.selects_row(row)
            || matches!(
                self,
                Self::Cell {
                    row: selected_row,
                    column: selected_column,
                    ..
                } if selected_row == row && selected_column == column
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnView {
    order: Vec<usize>,
    hidden: Vec<bool>,
    shown: usize,
    start: usize,
    visible: Vec<usize>,
}

impl ColumnView {
    fn new(total_columns: usize) -> Self {
        let mut view = Self {
            order: (0..total_columns).collect(),
            hidden: vec![false; total_columns],
            shown: total_columns,
            start: 0,
            visible: Vec::new(),
        };
        view.refresh();
        view
    }

    fn shown_count(&self) -> usize {
        self.shown
    }

    fn extend_to(&mut self, total_columns: usize) {
        let current = self.hidden.len();
        if total_columns <= current {
            return;
        }
        self.order.extend(current..total_columns);
        self.hidden.resize(total_columns, false);
        self.shown = self.shown.saturating_add(total_columns - current);
        self.refresh();
    }

    fn view(&mut self, column: usize) -> bool {
        if column >= self.hidden.len() {
            return false;
        }
        if self.hidden[column] {
            self.hidden[column] = false;
            self.shown = self.shown.saturating_add(1);
        }
        let Some(rank) = self
            .order
            .iter()
            .copied()
            .filter(|source| !self.hidden[*source])
            .position(|source| source == column)
        else {
            return false;
        };
        let maximum = self.shown_count().saturating_sub(MAX_VISIBLE_COLUMNS);
        self.start = rank.saturating_sub(MAX_VISIBLE_COLUMNS / 2).min(maximum);
        self.refresh();
        true
    }

    fn set_shown(&mut self, column: usize, shown: bool) -> bool {
        let Some(hidden) = self.hidden.get_mut(column) else {
            return false;
        };
        let was_shown = !*hidden;
        if was_shown != shown {
            self.shown = if shown {
                self.shown.saturating_add(1)
            } else {
                self.shown.saturating_sub(1)
            };
        }
        *hidden = !shown;
        self.refresh();
        true
    }

    fn move_column(&mut self, column: usize, target: usize) -> bool {
        let Some(position) = self.order.iter().position(|source| *source == column) else {
            return false;
        };
        if target >= self.order.len() {
            return false;
        }
        if position != target {
            let column = self.order.remove(position);
            self.order.insert(target, column);
        }
        self.refresh();
        true
    }

    fn first(&mut self) {
        self.start = 0;
        self.refresh();
    }

    fn reset(&mut self) {
        for (column, source) in self.order.iter_mut().enumerate() {
            *source = column;
        }
        self.hidden.fill(false);
        self.shown = self.hidden.len();
        self.first();
    }

    fn refresh(&mut self) {
        let maximum = self.shown_count().saturating_sub(MAX_VISIBLE_COLUMNS);
        self.start = self.start.min(maximum);
        self.visible = self
            .order
            .iter()
            .copied()
            .filter(|source| !self.hidden[*source])
            .skip(self.start)
            .take(MAX_VISIBLE_COLUMNS)
            .collect();
    }
}

struct Document {
    session: Session,
    job: Option<IndexJob>,
    index: Option<StructuralIndex>,
    progress: IndexProgress,
    search_job: Option<SearchJob>,
    search_query: Vec<u8>,
    last_match: Option<SearchMatch>,
    search_status: Option<String>,
    filter_job: Option<FilterJob>,
    filter_index: Option<FilterIndex>,
    filter_query: Option<FilterQuery>,
    filter_progress: Option<FilterProgress>,
    filter_status: Option<String>,
    export_job: Option<FilterExportJob>,
    export_progress: Option<FilterExportProgress>,
    export_status: Option<String>,
    export_cancel_requested: bool,
    save_as_job: Option<SaveAsJob>,
    save_as_status: Option<String>,
    save_as_cancel_requested: bool,
    filter_viewport_start: u64,
    filter_buffer_start: u64,
    filtered_rows: Vec<FilterMatch>,
    filter_read: Option<ActiveFilterRead>,
    pending_filter_read: Option<FilterReadWindow>,
    reveal_cell: Option<(u64, usize)>,
    selection: Option<GridSelection>,
    headers: Vec<String>,
    header_renames: BTreeMap<usize, String>,
    header_edit: Option<HeaderEdit>,
    total_columns: usize,
    columns: ColumnView,
    data_start: u64,
    viewport_start: u64,
    buffer_start: u64,
    buffered_rows: Vec<Row>,
    visible_rows: usize,
    scroll_points: f32,
    last_viewport_read: Option<Duration>,
    last_poll: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeaderEdit {
    column: usize,
    draft: String,
    focus_requested: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FilterReadWindow {
    start_match: u64,
    count: usize,
}

struct ActiveFilterRead {
    window: FilterReadWindow,
    job: FilterReadJob,
    started: Instant,
    cancel_requested: bool,
}

impl Document {
    #[cfg(test)]
    fn open(path: &Path, options: OpenOptions) -> Result<Self, String> {
        let mut document = Self::prepare(path, options)?;
        document.start_indexing()?;
        Ok(document)
    }

    fn prepare(path: &Path, options: OpenOptions) -> Result<Self, String> {
        let buffer_rows = BOOTSTRAP_ROWS + 2 * OVERSCAN_ROWS;
        let session = Session::open(
            path,
            OpenOptions {
                rows: buffer_rows + 1,
                ..options
            },
        )
        .map_err(|error| error.to_string())?;
        let data_start = u64::from(session.dialect.has_header);
        let total_columns = session
            .first_rows
            .iter()
            .map(|row| row.fields.len())
            .max()
            .unwrap_or(0);
        let columns = ColumnView::new(total_columns);
        let headers = headers_for(&session, &columns.visible);
        let buffered_rows = session
            .first_rows
            .iter()
            .skip(data_start as usize)
            .take(buffer_rows)
            .cloned()
            .collect();
        let progress = IndexProgress {
            bytes_scanned: 0,
            rows_scanned: 0,
            file_size: session.file_size,
            elapsed: Duration::ZERO,
            done: false,
            cancelled: false,
        };

        Ok(Self {
            session,
            job: None,
            index: None,
            progress,
            search_job: None,
            search_query: Vec::new(),
            last_match: None,
            search_status: None,
            filter_job: None,
            filter_index: None,
            filter_query: None,
            filter_progress: None,
            filter_status: None,
            export_job: None,
            export_progress: None,
            export_status: None,
            export_cancel_requested: false,
            save_as_job: None,
            save_as_status: None,
            save_as_cancel_requested: false,
            filter_viewport_start: 0,
            filter_buffer_start: 0,
            filtered_rows: Vec::new(),
            filter_read: None,
            pending_filter_read: None,
            reveal_cell: None,
            selection: None,
            headers,
            header_renames: BTreeMap::new(),
            header_edit: None,
            total_columns,
            columns,
            data_start,
            viewport_start: data_start,
            buffer_start: data_start,
            buffered_rows,
            visible_rows: BOOTSTRAP_ROWS,
            scroll_points: 0.0,
            last_viewport_read: None,
            last_poll: Instant::now() - POLL_INTERVAL,
        })
    }

    fn start_indexing(&mut self) -> Result<(), String> {
        let job = self
            .session
            .start_indexing(IndexConfig::default())
            .map_err(|error| error.to_string())?;
        self.progress = job.progress();
        self.job = Some(job);
        self.last_poll = Instant::now() - POLL_INTERVAL;
        Ok(())
    }

    fn poll(&mut self) -> Result<(), String> {
        let Some(job) = &self.job else {
            return Ok(());
        };
        let progress = job.progress();
        if self.last_poll.elapsed() < POLL_INTERVAL && !progress.done {
            return Ok(());
        }
        self.last_poll = Instant::now();
        self.progress = progress;
        if self.progress.done {
            let job = self.job.take().expect("index job is present");
            self.index = Some(job.wait().map_err(|error| error.to_string())?);
        }
        Ok(())
    }

    fn start_find_next(&mut self, query: &[u8]) -> Result<(), String> {
        if query.is_empty() {
            return Err("Enter text to find.".into());
        }
        if self.filter_active() {
            return Err("Clear the filter before using Find Next.".into());
        }
        if self.search_job.is_some() {
            return Err("A search is already running.".into());
        }
        if !self.is_search_ready() {
            return Err("Search is available after indexing completes.".into());
        }
        self.commit_header_edit();
        if self.search_query != query {
            self.search_query.clear();
            self.search_query.extend_from_slice(query);
            self.last_match = None;
        }
        let position = self.last_match.as_ref().map_or(
            SearchPosition {
                row: self.viewport_start,
                column: 0,
            },
            |found| SearchPosition {
                row: found.row,
                column: found.column.saturating_add(1),
            },
        );
        let index = self.index.as_ref().expect("index was checked above");
        self.search_job = Some(
            self.session
                .start_search(index, query.to_vec(), position)
                .map_err(|error| error.to_string())?,
        );
        self.search_status = None;
        self.reveal_cell = None;
        Ok(())
    }

    fn poll_search(&mut self) -> Result<(), String> {
        let Some(job) = &self.search_job else {
            return Ok(());
        };
        if !job.progress().done {
            return Ok(());
        }
        let job = self.search_job.take().expect("search job is present");
        match job.wait().map_err(|error| error.to_string())? {
            SearchOutcome::Match(found) => {
                let row = found.row;
                let column = found.column;
                self.navigate(row)?;
                self.center_column(column);
                self.reveal_cell = Some((row, column));
                self.search_status = Some(format!(
                    "Found row {}, column {}.",
                    row.saturating_sub(self.data_start).saturating_add(1),
                    column.saturating_add(1)
                ));
                self.last_match = Some(found);
            }
            SearchOutcome::NotFound => {
                self.search_status = Some("No further matches.".into());
            }
            SearchOutcome::Cancelled => {
                self.search_status = Some("Search cancelled.".into());
            }
        }
        Ok(())
    }

    fn search_progress(&self) -> Option<SearchProgress> {
        self.search_job.as_ref().map(SearchJob::progress)
    }

    fn start_filter(&mut self, query: FilterQuery) -> Result<(), String> {
        if self.export_job.is_some() {
            return Err("Cancel the active export before changing filters.".into());
        }
        if self.filter_job.is_some() {
            return Err("A filter is already running.".into());
        }
        if self.search_job.is_some() {
            return Err("Cancel the active search before filtering.".into());
        }
        if !self.is_filter_ready() {
            return Err("Open a file with at least one column before filtering.".into());
        }
        if query.predicates.is_empty() {
            return Err("Add at least one filter rule.".into());
        }
        for (index, predicate) in query.predicates.iter().enumerate() {
            self.validate_column(predicate.column)
                .map_err(|error| format!("Rule {}: {error}", index + 1))?;
            if predicate.operator == FilterOperator::Contains && predicate.value.is_empty() {
                return Err(format!(
                    "Rule {}: enter text for a Contains filter.",
                    index + 1
                ));
            }
        }

        let job = self
            .session
            .start_filter(query.clone())
            .map_err(|error| error.to_string())?;
        self.stop_filter_read();
        self.filter_progress = Some(job.progress());
        self.filter_index = Some(job.snapshot());
        self.filter_job = Some(job);
        self.filter_query = Some(query);
        self.filter_status = Some("Filtering rows…".into());
        self.export_progress = None;
        self.export_status = None;
        self.export_cancel_requested = false;
        self.filter_viewport_start = 0;
        self.filter_buffer_start = 0;
        self.filtered_rows.clear();
        self.selection = None;
        self.reveal_cell = None;
        self.scroll_points = 0.0;
        Ok(())
    }

    fn poll_filter(&mut self) -> Result<(), String> {
        self.poll_filter_scan()?;
        self.poll_filter_read()
    }

    fn poll_filter_scan(&mut self) -> Result<(), String> {
        let Some(job) = self.filter_job.as_ref() else {
            return Ok(());
        };
        let progress = job.progress();
        self.filter_progress = Some(progress);

        if progress.matches_found > 0 {
            let required = self
                .filter_viewport_start
                .saturating_add(self.visible_rows as u64)
                .min(progress.matches_found);
            self.refresh_filter_snapshot_for(required);
            self.navigate_filter(self.filter_viewport_start)?;
        } else {
            self.filtered_rows.clear();
            self.filter_buffer_start = 0;
            self.filter_viewport_start = 0;
        }
        if !progress.done {
            return Ok(());
        }

        let job = self.filter_job.take().expect("filter job is present");
        match job.wait() {
            Ok(index) => {
                let matches = index.matches_found();
                self.filter_index = Some(index);
                self.filter_status = Some(if progress.cancelled {
                    format!("Filter cancelled after {matches} matches.")
                } else if matches == 0 {
                    "Filter complete. No matching rows.".into()
                } else {
                    format!("Filter complete. {matches} matches.")
                });
                if matches > 0 {
                    self.navigate_filter(self.filter_viewport_start)?;
                }
                Ok(())
            }
            Err(error) => {
                self.stop_filter_read();
                self.filter_index = None;
                self.filter_query = None;
                self.filter_progress = None;
                self.filter_status = None;
                self.filtered_rows.clear();
                self.filter_viewport_start = 0;
                self.filter_buffer_start = 0;
                Err(error.to_string())
            }
        }
    }

    fn poll_filter_read(&mut self) -> Result<(), String> {
        let done = self
            .filter_read
            .as_ref()
            .is_some_and(|active| active.job.progress().done);
        if !done {
            return Ok(());
        }

        let active = self
            .filter_read
            .take()
            .expect("completed filter read should be active");
        let desired = self.filter_read_window(self.filter_viewport_start);
        match active.job.wait() {
            Ok(FilterReadOutcome::Complete(rows)) if desired == Some(active.window) => {
                let loaded_columns = rows.iter().map(|row| row.fields.len()).max().unwrap_or(0);
                if loaded_columns > self.total_columns {
                    self.ensure_column_count(loaded_columns);
                    self.refresh_column_headers();
                }
                self.last_viewport_read = Some(active.started.elapsed());
                self.filter_buffer_start = active.window.start_match;
                self.filtered_rows = rows;
            }
            Ok(FilterReadOutcome::Complete(_) | FilterReadOutcome::Cancelled) => {}
            Err(error) => {
                self.pending_filter_read = None;
                return Err(error.to_string());
            }
        }

        self.schedule_filter_buffer(self.filter_viewport_start)
    }

    fn start_filtered_export(&mut self, destination: PathBuf) -> Result<(), String> {
        if self.save_as_job.is_some() {
            return Err("Cancel the active Save As before exporting filtered rows.".into());
        }
        self.commit_header_edit();
        if self.is_dirty() {
            return Err("Save or discard your changes before exporting filtered rows.".into());
        }
        if self.export_job.is_some() {
            return Err("A filtered export is already running.".into());
        }
        let query = self
            .filter_query
            .clone()
            .ok_or_else(|| "Apply a filter before exporting rows.".to_owned())?;
        let progress = self
            .filter_progress()
            .ok_or_else(|| "Wait for filtering to complete before exporting.".to_owned())?;
        if self.filter_job.is_some() || !progress.done {
            return Err("Wait for filtering to complete before exporting.".into());
        }
        if progress.cancelled {
            return Err("Run the filter to completion before exporting.".into());
        }

        let job = self
            .session
            .start_filtered_export(query, destination)
            .map_err(|error| error.to_string())?;
        self.export_progress = Some(job.progress());
        self.export_job = Some(job);
        self.export_status = Some(
            "Export in progress. To open or reapply a file, cancel this export and wait for it to finish."
                .into(),
        );
        self.export_cancel_requested = false;
        Ok(())
    }

    fn poll_filtered_export(&mut self) -> Result<(), String> {
        let Some(job) = self.export_job.as_ref() else {
            return Ok(());
        };
        let progress = job.progress();
        self.export_progress = Some(progress);
        if !progress.done {
            return Ok(());
        }

        let job = self
            .export_job
            .take()
            .expect("filter export job is present");
        self.export_cancel_requested = false;
        match job.wait() {
            Ok(FilterExportOutcome::Complete(summary)) => {
                self.export_status = Some(format!(
                    "Export complete: {} rows ({}) saved to {}.",
                    summary.rows_written,
                    format_bytes(summary.bytes_written),
                    summary.destination.display()
                ));
                Ok(())
            }
            Ok(FilterExportOutcome::Cancelled) => {
                self.export_status = Some("Export cancelled. No output file was created.".into());
                Ok(())
            }
            Err(error) => {
                self.export_status = Some("Export failed. No output file was created.".into());
                Err(error.to_string())
            }
        }
    }

    fn cancel_filtered_export(&mut self) {
        if let Some(job) = &self.export_job {
            job.cancel();
            self.export_cancel_requested = true;
            self.export_status = Some(
                "Cancelling export… Wait for it to finish before opening or reapplying the file."
                    .into(),
            );
        }
    }

    fn filtered_export_progress(&self) -> Option<FilterExportProgress> {
        self.export_job
            .as_ref()
            .map(FilterExportJob::progress)
            .or(self.export_progress)
    }

    fn is_filtered_export_ready(&self) -> bool {
        self.filter_query.is_some()
            && self.filter_job.is_none()
            && self.export_job.is_none()
            && self.save_as_job.is_none()
            && self
                .filter_progress
                .is_some_and(|progress| progress.done && !progress.cancelled)
    }

    fn is_save_as_ready(&self) -> bool {
        self.is_dirty() && self.save_as_job.is_none() && self.export_job.is_none()
    }

    fn start_save_as(&mut self, destination: PathBuf) -> Result<(), String> {
        if self.save_as_job.is_some() {
            return Err("A Save As operation is already running.".into());
        }
        if self.export_job.is_some() {
            return Err("Cancel the active export before using Save As.".into());
        }
        self.commit_header_edit();
        if !self.is_dirty() {
            return Err("Rename a header before using Save As.".into());
        }
        let renames = self
            .header_renames
            .iter()
            .map(|(column, name)| (*column, name.as_bytes().to_vec()))
            .collect();
        let job = self
            .session
            .start_save_as_with_header_renames(renames, destination)
            .map_err(|error| error.to_string())?;
        self.save_as_job = Some(job);
        self.save_as_status = Some("Saving edited file…".into());
        self.save_as_cancel_requested = false;
        Ok(())
    }

    fn poll_save_as(&mut self) -> Result<Option<PathBuf>, String> {
        let Some(job) = self.save_as_job.as_ref() else {
            return Ok(None);
        };
        let progress = job.progress();
        if !progress.done {
            return Ok(None);
        }

        let job = self.save_as_job.take().expect("Save As job is present");
        self.save_as_cancel_requested = false;
        match job.wait() {
            Ok(SaveAsOutcome::Complete(summary)) => Ok(Some(summary.destination)),
            Ok(SaveAsOutcome::Cancelled) => {
                self.save_as_status = Some("Save As cancelled. No output file was created.".into());
                Ok(None)
            }
            Err(error) => {
                self.save_as_status = Some("Save As failed. No output file was created.".into());
                Err(error.to_string())
            }
        }
    }

    fn cancel_save_as(&mut self) {
        if let Some(job) = &self.save_as_job {
            job.cancel();
            self.save_as_cancel_requested = true;
            self.save_as_status = Some("Cancelling Save As…".into());
        }
    }

    fn cancel_filter(&self) {
        if let Some(job) = &self.filter_job {
            job.cancel();
        }
    }

    fn clear_filter(&mut self) -> Result<(), String> {
        if self.export_job.is_some() {
            return Err("Cancel the active export before clearing filters.".into());
        }
        self.stop_filter_read();
        if let Some(job) = self.filter_job.take() {
            job.cancel();
            drop(job);
        }
        self.filter_index = None;
        self.filter_query = None;
        self.filter_progress = None;
        self.filter_status = None;
        self.export_progress = None;
        self.export_status = None;
        self.export_cancel_requested = false;
        self.filter_viewport_start = 0;
        self.filter_buffer_start = 0;
        self.filtered_rows.clear();
        self.selection = None;
        self.reveal_cell = None;
        self.scroll_points = 0.0;
        if self.available_data_rows() > 0 {
            self.navigate(self.viewport_start)?;
        }
        Ok(())
    }

    fn filter_active(&self) -> bool {
        self.filter_query.is_some()
    }

    fn is_filter_ready(&self) -> bool {
        self.total_columns > 0
    }

    fn filter_progress(&self) -> Option<FilterProgress> {
        self.filter_job
            .as_ref()
            .map(FilterJob::progress)
            .or(self.filter_progress)
    }

    fn filter_rows_loading(&self) -> bool {
        self.filter_read.is_some() || self.pending_filter_read.is_some()
    }

    fn filter_empty_message(&self, row_count: usize) -> Option<&'static str> {
        if !self.filter_active() || row_count != 0 {
            None
        } else if self.available_filter_rows() == 0 && self.filter_job.is_some() {
            Some("Finding matching rows…")
        } else if self.filter_rows_loading() {
            Some("Loading matching rows…")
        } else {
            Some("No matching rows")
        }
    }

    fn available_filter_rows(&self) -> u64 {
        self.filter_progress
            .map(|progress| progress.matches_found)
            .unwrap_or(0)
            .max(
                self.filter_index
                    .as_ref()
                    .map(FilterIndex::matches_found)
                    .unwrap_or(0),
            )
    }

    fn refresh_filter_snapshot_for(&mut self, required_matches: u64) {
        let indexed = self
            .filter_index
            .as_ref()
            .map(FilterIndex::matches_found)
            .unwrap_or(0);
        if indexed >= required_matches {
            return;
        }
        if let Some(job) = &self.filter_job {
            self.filter_index = Some(job.snapshot());
        }
    }

    fn is_search_ready(&self) -> bool {
        self.index.is_some() && self.progress.done && !self.progress.cancelled
    }

    fn cancel_search(&self) {
        if let Some(job) = &self.search_job {
            job.cancel();
        }
    }

    fn center_column(&mut self, column: usize) {
        self.commit_header_edit();
        self.ensure_column_count(column.saturating_add(1));
        self.columns.view(column);
        self.refresh_column_headers();
        self.clear_hidden_selection();
    }

    fn view_column(&mut self, column: usize) -> Result<(), String> {
        self.commit_header_edit();
        self.validate_column(column)?;
        self.columns.view(column);
        self.refresh_column_headers();
        self.reveal_cell = Some((self.current_source_row(), column));
        self.clear_hidden_selection();
        Ok(())
    }

    fn set_column_shown(&mut self, column: usize, shown: bool) -> Result<(), String> {
        self.commit_header_edit();
        self.validate_column(column)?;
        self.columns.set_shown(column, shown);
        self.refresh_column_headers();
        if !shown
            && self
                .reveal_cell
                .is_some_and(|(_, revealed)| revealed == column)
        {
            self.reveal_cell = None;
        }
        self.clear_hidden_selection();
        Ok(())
    }

    fn move_column(&mut self, column: usize, position: usize) -> Result<(), String> {
        self.commit_header_edit();
        self.validate_column(column)?;
        if position >= self.total_columns {
            return Err(format!(
                "Display position must be between 1 and {}.",
                self.total_columns
            ));
        }
        self.columns.move_column(column, position);
        self.refresh_column_headers();
        self.clear_hidden_selection();
        Ok(())
    }

    fn show_first_columns(&mut self) {
        self.commit_header_edit();
        self.columns.first();
        self.refresh_column_headers();
        self.reveal_cell = self
            .columns
            .visible
            .first()
            .copied()
            .map(|column| (self.current_source_row(), column));
        self.clear_hidden_selection();
    }

    fn reset_columns(&mut self) {
        self.commit_header_edit();
        self.columns.reset();
        self.refresh_column_headers();
        self.reveal_cell = self
            .columns
            .visible
            .first()
            .copied()
            .map(|column| (self.current_source_row(), column));
        self.clear_hidden_selection();
    }

    fn ensure_column_count(&mut self, total_columns: usize) {
        if total_columns > self.total_columns {
            self.total_columns = total_columns;
            self.columns.extend_to(total_columns);
        }
    }

    fn refresh_column_headers(&mut self) {
        self.headers = self
            .columns
            .visible
            .iter()
            .map(|column| self.column_name(*column))
            .collect();
    }

    fn column_name(&self, column: usize) -> String {
        self.header_renames
            .get(&column)
            .map(|name| field_text(name.as_bytes()))
            .unwrap_or_else(|| column_name(&self.session, column))
    }

    fn header_is_editable(&self, column: usize) -> bool {
        self.save_as_job.is_none()
            && self.export_job.is_none()
            && self.search_job.is_none()
            && self.source_header_name(column).is_some()
    }

    fn source_header_name(&self, column: usize) -> Option<&str> {
        if !self.session.dialect.has_header {
            return None;
        }
        let field = self.session.first_rows.first()?.fields.get(column)?;
        let name = std::str::from_utf8(field).ok()?;
        Some(if column == 0 {
            name.strip_prefix('\u{feff}').unwrap_or(name)
        } else {
            name
        })
    }

    fn begin_header_edit(&mut self, column: usize) {
        if !self.header_is_editable(column) {
            return;
        }
        if let Some(source_name) = self.source_header_name(column) {
            let draft = self
                .header_renames
                .get(&column)
                .cloned()
                .unwrap_or_else(|| source_name.to_owned());
            self.commit_header_edit();
            self.header_edit = Some(HeaderEdit {
                column,
                draft,
                focus_requested: true,
            });
        }
    }

    fn rename_header(&mut self, column: usize, name: String) -> Result<(), String> {
        if self.save_as_job.is_some() {
            return Err("Wait for Save As to finish before editing headers.".into());
        }
        if self.export_job.is_some() {
            return Err("Wait for the filtered export to finish before editing headers.".into());
        }
        if self.search_job.is_some() {
            return Err("Wait for the search to finish before editing headers.".into());
        }
        if !self.header_is_editable(column) {
            return Err("Only columns in the source header row can be renamed.".into());
        }
        if name.as_bytes()
            == self
                .source_header_name(column)
                .expect("editable header has source text")
                .as_bytes()
        {
            self.header_renames.remove(&column);
        } else {
            self.header_renames.insert(column, name);
        }
        self.refresh_column_headers();
        Ok(())
    }

    fn commit_header_edit(&mut self) {
        if self.save_as_job.is_some() {
            return;
        }
        if let Some(edit) = self.header_edit.take() {
            let _ = self.rename_header(edit.column, edit.draft);
        }
    }

    fn discard_header_edits(&mut self) {
        if self.save_as_job.is_some() {
            return;
        }
        self.header_edit = None;
        self.header_renames.clear();
        self.refresh_column_headers();
    }

    fn is_dirty(&self) -> bool {
        !self.header_renames.is_empty()
    }

    fn validate_column(&self, column: usize) -> Result<(), String> {
        if column < self.total_columns {
            Ok(())
        } else if self.total_columns == 0 {
            Err("This file has no columns.".into())
        } else {
            Err(format!(
                "File column must be between 1 and {}.",
                self.total_columns
            ))
        }
    }

    fn is_indexing(&self) -> bool {
        self.job.is_some() && !self.progress.done
    }

    fn index_status(&self) -> &'static str {
        if self.job.is_none() && self.index.is_none() {
            "Index failed"
        } else if self.progress.cancelled {
            "Index cancelled"
        } else if self.job.is_some() {
            "Indexing"
        } else {
            "Index complete"
        }
    }

    fn shutdown(&mut self) {
        self.stop_filter_read();
        if let Some(job) = self.save_as_job.take() {
            job.cancel();
            drop(job);
        }
        self.save_as_cancel_requested = false;
        if let Some(job) = self.export_job.take() {
            job.cancel();
            drop(job);
        }
        self.export_cancel_requested = false;
        if let Some(job) = self.filter_job.take() {
            job.cancel();
            drop(job);
        }
        if let Some(job) = self.search_job.take() {
            job.cancel();
            drop(job);
        }
        if let Some(job) = self.job.take() {
            drop(job);
        }
    }

    fn stop_filter_read(&mut self) {
        self.pending_filter_read = None;
        if let Some(active) = self.filter_read.take() {
            active.job.cancel_without_waiting();
        }
    }

    fn cancel(&self) {
        if let Some(job) = &self.job {
            job.cancel();
        }
    }

    fn available_rows(&self) -> u64 {
        self.index
            .as_ref()
            .map(StructuralIndex::indexed_rows)
            .unwrap_or(self.progress.rows_scanned)
            .max(self.session.first_rows.len() as u64)
    }

    fn available_data_rows(&self) -> u64 {
        self.available_rows().saturating_sub(self.data_start)
    }

    fn indexed_fraction(&self) -> f32 {
        if self.session.file_size == 0 && (self.progress.done || self.index.is_some()) {
            1.0
        } else {
            self.progress.bytes_scanned as f32 / self.session.file_size.max(1) as f32
        }
    }

    fn set_visible_rows(&mut self, visible_rows: usize) -> Result<(), String> {
        self.visible_rows = visible_rows.max(1);
        if self.filter_active() {
            return self.navigate_filter(self.filter_viewport_start);
        }
        if self.available_data_rows() == 0 {
            return Ok(());
        }
        let target = self.reveal_cell.map_or(self.viewport_start, |(row, _)| row);
        self.navigate(target)
    }

    fn page(&mut self, direction: i64) -> Result<(), String> {
        let page = i64::try_from(self.visible_rows).unwrap_or(i64::MAX);
        self.scroll_rows(direction.saturating_mul(page))
    }

    fn scroll_by_points(&mut self, delta_y: f32, row_stride: f32) -> Result<(), String> {
        let row_stride = row_stride.max(f32::EPSILON);
        self.scroll_points -= delta_y;
        let rows = (self.scroll_points / row_stride).trunc() as i64;
        if rows == 0 {
            return Ok(());
        }
        self.scroll_points -= rows as f32 * row_stride;
        self.scroll_rows(rows)
    }

    fn scroll_rows(&mut self, rows: i64) -> Result<(), String> {
        if self.filter_active() {
            let available = self.available_filter_rows();
            let maximum = max_viewport_start(available, self.visible_rows);
            let current = self.filter_viewport_start;
            let target = if rows.is_negative() {
                current.saturating_sub(rows.unsigned_abs())
            } else {
                current.saturating_add(rows as u64).min(maximum)
            };
            if target == current {
                self.scroll_points = 0.0;
                return Ok(());
            }
            return self.navigate_filter(target);
        }
        let available = self.available_data_rows();
        let maximum = max_viewport_start(available, self.visible_rows);
        let current = self.viewport_start.saturating_sub(self.data_start);
        let target = if rows.is_negative() {
            current.saturating_sub(rows.unsigned_abs())
        } else {
            current.saturating_add(rows as u64).min(maximum)
        };
        if target == current {
            self.scroll_points = 0.0;
            return Ok(());
        }
        self.navigate(self.data_start.saturating_add(target))
    }

    fn navigate_filter(&mut self, requested: u64) -> Result<(), String> {
        let available = self.available_filter_rows();
        if available == 0 {
            self.cancel_filter_read_for_navigation();
            self.filter_viewport_start = 0;
            self.filter_buffer_start = 0;
            self.filtered_rows.clear();
            self.clear_hidden_selection();
            return Ok(());
        }
        let start = requested.min(max_viewport_start(available, self.visible_rows));
        self.schedule_filter_buffer(start)?;
        self.filter_viewport_start = start;
        self.clear_hidden_selection();
        Ok(())
    }

    fn navigate(&mut self, requested: u64) -> Result<(), String> {
        let available = self.available_rows();
        if requested.max(self.data_start) >= available {
            return Err(format!(
                "Data row {} is not indexed yet ({} available).",
                requested.saturating_sub(self.data_start).saturating_add(1),
                self.available_data_rows()
            ));
        }
        let start = logical_viewport_start(
            requested,
            self.data_start,
            self.available_data_rows(),
            self.visible_rows,
        );
        self.load_buffer(start)?;
        self.viewport_start = start;
        self.clear_hidden_selection();
        Ok(())
    }

    fn load_buffer(&mut self, viewport_start: u64) -> Result<(), String> {
        let available = self.available_rows();
        let visible_end = viewport_start
            .saturating_add((available - viewport_start).min(self.visible_rows as u64));
        let loaded_end = self
            .buffer_start
            .saturating_add(self.buffered_rows.len() as u64);
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        if self.buffer_start <= viewport_start
            && loaded_end >= visible_end
            && self.buffered_rows.len() as u64 <= capacity
        {
            return Ok(());
        }

        let last_start = available.saturating_sub(capacity).max(self.data_start);
        let start = viewport_start
            .saturating_sub(OVERSCAN_ROWS as u64)
            .max(self.data_start)
            .min(last_start);
        let count = (available - start).min(capacity) as usize;
        let end = start.saturating_add(count as u64);
        let began = Instant::now();
        let rows = if end <= self.session.first_rows.len() as u64 {
            self.session.first_rows[start as usize..end as usize].to_vec()
        } else if let Some(index) = &self.index {
            self.session
                .read_rows(index, start, count)
                .map_err(|error| error.to_string())?
        } else if let Some(job) = &self.job {
            self.session
                .read_rows(&job.snapshot(), start, count)
                .map_err(|error| error.to_string())?
        } else {
            return Err("No structural index is available.".into());
        };
        let loaded_columns = rows.iter().map(|row| row.fields.len()).max().unwrap_or(0);
        if loaded_columns > self.total_columns {
            self.ensure_column_count(loaded_columns);
            self.refresh_column_headers();
        }
        self.last_viewport_read = Some(began.elapsed());
        self.buffer_start = start;
        self.buffered_rows = rows;
        Ok(())
    }

    fn filter_read_window(&self, viewport_start: u64) -> Option<FilterReadWindow> {
        let available = self.available_filter_rows();
        if available == 0 {
            return None;
        }
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        let last_start = available.saturating_sub(capacity);
        let start_match = viewport_start
            .saturating_sub(OVERSCAN_ROWS as u64)
            .min(last_start);
        let count = (available - start_match).min(capacity) as usize;
        Some(FilterReadWindow { start_match, count })
    }

    fn filter_buffer_covers(&self, viewport_start: u64) -> bool {
        let available = self.available_filter_rows();
        if available == 0 {
            return false;
        }
        let visible_end = viewport_start
            .saturating_add((available - viewport_start).min(self.visible_rows as u64));
        let loaded_end = self
            .filter_buffer_start
            .saturating_add(self.filtered_rows.len() as u64);
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        self.filter_buffer_start <= viewport_start
            && loaded_end >= visible_end
            && self.filtered_rows.len() as u64 <= capacity
    }

    fn schedule_filter_buffer(&mut self, viewport_start: u64) -> Result<(), String> {
        let Some(window) = self.filter_read_window(viewport_start) else {
            self.cancel_filter_read_for_navigation();
            self.filter_buffer_start = 0;
            self.filtered_rows.clear();
            return Ok(());
        };

        if self.filtered_rows.len() > window.count && window.start_match >= self.filter_buffer_start
        {
            let offset =
                usize::try_from(window.start_match.saturating_sub(self.filter_buffer_start))
                    .unwrap_or(self.filtered_rows.len());
            if offset.saturating_add(window.count) <= self.filtered_rows.len() {
                self.filtered_rows.drain(..offset);
                self.filtered_rows.truncate(window.count);
                self.filter_buffer_start = window.start_match;
            }
        }

        if self.filter_buffer_covers(viewport_start) {
            self.cancel_filter_read_for_navigation();
            return Ok(());
        }

        self.refresh_filter_snapshot_for(window.start_match.saturating_add(window.count as u64));
        if let Some(active) = self.filter_read.as_mut() {
            if active.window == window && !active.cancel_requested {
                self.pending_filter_read = None;
            } else {
                if !active.cancel_requested {
                    active.job.cancel();
                    active.cancel_requested = true;
                }
                self.pending_filter_read = Some(window);
            }
            return Ok(());
        }

        self.start_filter_read(window)
    }

    fn start_filter_read(&mut self, window: FilterReadWindow) -> Result<(), String> {
        self.pending_filter_read = None;
        let index = self
            .filter_index
            .as_ref()
            .ok_or_else(|| "No filter index is available.".to_owned())?;
        let job = self
            .session
            .start_filtered_read(index, window.start_match, window.count)
            .map_err(|error| error.to_string())?;
        self.filter_read = Some(ActiveFilterRead {
            window,
            job,
            started: Instant::now(),
            cancel_requested: false,
        });
        Ok(())
    }

    fn cancel_filter_read_for_navigation(&mut self) {
        self.pending_filter_read = None;
        if let Some(active) = self.filter_read.as_mut()
            && !active.cancel_requested
        {
            active.job.cancel();
            active.cancel_requested = true;
        }
    }

    fn visible_rows(&self) -> &[Row] {
        let offset = usize::try_from(self.viewport_start.saturating_sub(self.buffer_start))
            .unwrap_or(self.buffered_rows.len())
            .min(self.buffered_rows.len());
        let end = offset
            .saturating_add(self.visible_rows)
            .min(self.buffered_rows.len());
        &self.buffered_rows[offset..end]
    }

    fn visible_filter_rows(&self) -> &[FilterMatch] {
        let offset = usize::try_from(
            self.filter_viewport_start
                .saturating_sub(self.filter_buffer_start),
        )
        .unwrap_or(self.filtered_rows.len())
        .min(self.filtered_rows.len());
        let end = offset
            .saturating_add(self.visible_rows)
            .min(self.filtered_rows.len());
        &self.filtered_rows[offset..end]
    }

    fn visible_row_count(&self) -> usize {
        if self.filter_active() {
            self.visible_filter_rows().len()
        } else {
            self.visible_rows().len()
        }
    }

    fn visible_row(&self, index: usize) -> Option<(u64, &[Vec<u8>])> {
        if self.filter_active() {
            self.visible_filter_rows()
                .get(index)
                .map(|row| (row.row, row.fields.as_slice()))
        } else {
            self.visible_rows().get(index).map(|row| {
                (
                    self.viewport_start.saturating_add(index as u64),
                    row.fields.as_slice(),
                )
            })
        }
    }

    fn current_source_row(&self) -> u64 {
        self.visible_row(0)
            .map(|(row, _)| row)
            .unwrap_or(self.viewport_start)
    }

    fn clear_hidden_selection(&mut self) {
        let Some(selection) = self.selection else {
            return;
        };
        let row_visible = if self.filter_active() {
            self.visible_filter_rows()
                .iter()
                .any(|row| row.row == selection.row())
        } else {
            let row_end = self
                .viewport_start
                .saturating_add(self.visible_rows().len() as u64);
            (self.viewport_start..row_end).contains(&selection.row())
        };
        let column_visible = match selection {
            GridSelection::Row { .. } => true,
            GridSelection::Cell { column, .. } => self.columns.visible.contains(&column),
        };
        if !row_visible || !column_visible {
            self.selection = None;
        }
    }

    fn copy_selection_text(&self) -> Result<String, String> {
        let selection = self
            .selection
            .ok_or_else(|| "Select a cell or row before copying.".to_owned())?;
        if self.filter_active() {
            let row = self
                .visible_filter_rows()
                .iter()
                .find(|row| row.row == selection.row())
                .ok_or_else(|| {
                    "The selected row is no longer visible. Select it again.".to_owned()
                })?;
            selection_fields_text(&row.fields, selection, MAX_COPY_BYTES)
        } else {
            let relative = selection
                .row()
                .checked_sub(self.buffer_start)
                .ok_or_else(|| {
                    "The selected row is no longer visible. Select it again.".to_owned()
                })?;
            let offset = usize::try_from(relative).map_err(|_| {
                "The selected row is no longer visible. Select it again.".to_owned()
            })?;
            let row = self.buffered_rows.get(offset).ok_or_else(|| {
                "The selected row is no longer visible. Select it again.".to_owned()
            })?;
            selection_text(row, selection, MAX_COPY_BYTES)
        }
    }

    fn grid_total_rows(&self) -> u64 {
        if self.filter_active() {
            self.available_filter_rows()
        } else {
            self.available_data_rows()
        }
    }

    fn grid_position(&self) -> u64 {
        if self.filter_active() {
            self.filter_viewport_start
        } else {
            self.viewport_start.saturating_sub(self.data_start)
        }
    }

    fn navigate_grid_position(&mut self, position: u64) -> Result<(), String> {
        if self.filter_active() {
            self.navigate_filter(position)
        } else {
            self.navigate(self.data_start.saturating_add(position))
        }
    }

    fn display_start(&self) -> u64 {
        if self.available_data_rows() == 0 {
            return 0;
        }
        self.viewport_start
            .saturating_sub(self.data_start)
            .saturating_add(1)
    }

    fn display_end(&self) -> u64 {
        if self.visible_rows().is_empty() {
            return 0;
        }
        self.display_start()
            .saturating_add(self.visible_rows().len().saturating_sub(1) as u64)
    }
}

fn max_viewport_start(total_rows: u64, visible_rows: usize) -> u64 {
    total_rows.saturating_sub(visible_rows as u64)
}

fn logical_viewport_start(
    requested: u64,
    data_start: u64,
    total_rows: u64,
    visible_rows: usize,
) -> u64 {
    data_start.saturating_add(
        requested
            .saturating_sub(data_start)
            .min(max_viewport_start(total_rows, visible_rows)),
    )
}

fn scroll_fraction_for_row(row: u64, data_start: u64, total_rows: u64, visible_rows: usize) -> f64 {
    let maximum = max_viewport_start(total_rows, visible_rows);
    if maximum == 0 {
        0.0
    } else {
        row.saturating_sub(data_start).min(maximum) as f64 / maximum as f64
    }
}

fn row_for_scroll_fraction(
    fraction: f64,
    data_start: u64,
    total_rows: u64,
    visible_rows: usize,
) -> u64 {
    let maximum = max_viewport_start(total_rows, visible_rows);
    let relative = (fraction.clamp(0.0, 1.0) * maximum as f64).round() as u64;
    data_start.saturating_add(relative.min(maximum))
}

fn parse_data_row(value: &str, data_start: u64) -> Result<u64, String> {
    let row: u64 = value
        .trim()
        .parse()
        .map_err(|_| "Data row must be a positive whole number.".to_owned())?;
    if row == 0 {
        return Err("Data rows start at 1.".into());
    }
    Ok(data_start.saturating_add(row - 1))
}

fn parse_file_column(value: &str, total_columns: usize) -> Result<usize, String> {
    let column: usize = value
        .trim()
        .parse()
        .map_err(|_| "File column must be a positive whole number.".to_owned())?;
    if column == 0 {
        return Err("File columns start at 1.".into());
    }
    if column > total_columns {
        return Err(if total_columns == 0 {
            "This file has no columns.".into()
        } else {
            format!("File column must be between 1 and {total_columns}.")
        });
    }
    Ok(column - 1)
}

fn parse_column_position(value: &str, total_columns: usize) -> Result<usize, String> {
    let position: usize = value
        .trim()
        .parse()
        .map_err(|_| "Display position must be a positive whole number.".to_owned())?;
    if position == 0 {
        return Err("Display positions start at 1.".into());
    }
    if position > total_columns {
        return Err(if total_columns == 0 {
            "This file has no columns.".into()
        } else {
            format!("Display position must be between 1 and {total_columns}.")
        });
    }
    Ok(position - 1)
}

fn headers_for(session: &Session, columns: &[usize]) -> Vec<String> {
    columns
        .iter()
        .map(|column| column_name(session, *column))
        .collect()
}

fn column_name(session: &Session, column: usize) -> String {
    if session.dialect.has_header {
        let text = session
            .first_rows
            .first()
            .and_then(|row| row.fields.get(column))
            .map_or_else(String::new, |field| field_text(field));
        if !text.is_empty() {
            return text;
        }
    }
    format!("Column {}", column + 1)
}

fn show_grid(ui: &mut egui::Ui, document: &mut Document) -> Result<(), String> {
    let grid_height = ui.available_height();
    let horizontal_scrollbar = ui.spacing().scroll.allocated_width();
    let body_height =
        (grid_height - GRID_TITLE_HEIGHT - HEADER_HEIGHT - horizontal_scrollbar).max(ROW_HEIGHT);
    let row_stride = ROW_HEIGHT;
    let visible_rows = (body_height / row_stride).floor().max(1.0) as usize;
    document.set_visible_rows(visible_rows)?;
    let total_rows = document.grid_total_rows();

    let grid_rect = ui.available_rect_before_wrap();
    if total_rows > 0 && ui.rect_contains_pointer(grid_rect) {
        let delta_y = ui.input_mut(|input| {
            let delta_y = input.smooth_scroll_delta.y;
            input.smooth_scroll_delta.y = 0.0;
            delta_y
        });
        if delta_y != 0.0 {
            document.scroll_by_points(delta_y, row_stride)?;
        }
    }

    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        let position = document.grid_position();
        let fraction = scroll_fraction_for_row(position, 0, total_rows, document.visible_rows);
        let mut slider_position = 1.0 - fraction;
        let thumb_height = scrollbar_thumb_height(grid_height, total_rows, document.visible_rows);
        let handle_radius = SCROLLBAR_WIDTH / 2.5;
        let scroll_enabled = total_rows > 0;
        let label = if scroll_enabled && document.filter_active() {
            format!(
                "Vertical scroll, filter match {} of {total_rows}",
                position.saturating_add(1)
            )
        } else if scroll_enabled {
            format!(
                "Vertical scroll, row {} of {total_rows}",
                document.display_start()
            )
        } else {
            "Vertical scroll, no data rows".into()
        };
        let hover_text = if scroll_enabled && document.filter_active() {
            format!(
                "Filter match {} of {total_rows}",
                position.saturating_add(1)
            )
        } else if scroll_enabled {
            format!("Row {} of {total_rows}", document.display_start())
        } else {
            "No data rows".into()
        };
        let response = ui
            .allocate_ui_with_layout(
                egui::vec2(SCROLLBAR_WIDTH, grid_height),
                Layout::top_down(Align::Center),
                |ui| {
                    ui.spacing_mut().slider_width = grid_height;
                    ui.add_enabled(
                        scroll_enabled,
                        egui::Slider::new(&mut slider_position, 0.0..=1.0)
                            .vertical()
                            .show_value(false)
                            .smart_aim(false)
                            .handle_shape(egui::style::HandleShape::Rect {
                                aspect_ratio: thumb_height / (2.0 * handle_radius),
                            }),
                    )
                },
            )
            .inner
            .on_hover_text(hover_text);
        response.widget_info(|| {
            egui::WidgetInfo::slider(scroll_enabled && ui.is_enabled(), slider_position, &label)
        });
        if scroll_enabled && response.changed() {
            let target = row_for_scroll_fraction(
                1.0 - slider_position,
                0,
                total_rows,
                document.visible_rows,
            );
            if target != position {
                document.navigate_grid_position(target)?;
            }
        }

        let reveal_cell = document.reveal_cell.take();
        ui.separator();
        let selection = ui
            .allocate_ui_with_layout(ui.available_size(), Layout::top_down(Align::Min), |ui| {
                show_table(ui, document, reveal_cell)
            })
            .inner;
        if let Some(selection) = selection {
            document.selection = Some(selection);
        }
        Ok(())
    })
    .inner
}

fn show_table(
    ui: &mut egui::Ui,
    document: &mut Document,
    reveal_cell: Option<(u64, usize)>,
) -> Option<GridSelection> {
    let row_count = document.visible_row_count();
    let mut clicked_selection = None;
    let mut active_header_edit = document.header_edit.take();
    let mut begin_header_edit = None;
    let mut commit_header_edit = false;
    let mut cancel_header_edit = false;
    ui.horizontal(|ui| {
        if let Some(message) = document.filter_empty_message(row_count) {
            ui.heading(message);
        } else if document.filter_active() {
            ui.heading(format!(
                "Matches {}–{} of {}",
                document.filter_viewport_start.saturating_add(1),
                document
                    .filter_viewport_start
                    .saturating_add(row_count as u64),
                document.available_filter_rows()
            ));
        } else if row_count == 0 {
            ui.heading("No data rows");
        } else {
            ui.heading(format!(
                "Rows {}–{}",
                document.display_start(),
                document.display_end()
            ));
        }
        let shown_columns = document.columns.shown_count();
        if shown_columns == 0 && document.total_columns > 0 {
            ui.label(format!("All {} columns hidden", document.total_columns));
        } else if shown_columns > 0 {
            ui.label(format!(
                "View columns {}–{} of {} shown ({} total)",
                document.columns.start.saturating_add(1),
                document
                    .columns
                    .start
                    .saturating_add(document.headers.len()),
                shown_columns,
                document.total_columns
            ));
        }
    });
    ui.add_space(6.0);

    let grid_height = ui.available_height();
    let viewport_width = ui.available_width();
    let column_width =
        ((viewport_width - 82.0) / document.headers.len().max(1) as f32).clamp(80.0, 160.0);
    let content_width =
        74.0 + document.headers.len() as f32 * (column_width + ui.spacing().item_spacing.x);
    let body_height =
        (grid_height - HEADER_HEIGHT - ui.spacing().scroll.allocated_width()).max(ROW_HEIGHT);

    egui::ScrollArea::horizontal()
        .id_salt("quarry-grid-horizontal")
        .auto_shrink([false, false])
        .max_height(grid_height)
        .show(ui, |ui| {
            ui.set_min_width(content_width.max(viewport_width));
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut table = TableBuilder::new(ui)
                .id_salt("quarry-grid")
                .striped(true)
                .resizable(true)
                .vscroll(false)
                .auto_shrink([false, false])
                .min_scrolled_height(body_height)
                .max_scroll_height(body_height)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::exact(74.0).clip(true));
            for _ in &document.headers {
                table = table.column(
                    Column::initial(column_width)
                        .at_least(80.0)
                        .clip(true)
                        .resizable(true),
                );
            }
            table
                .header(HEADER_HEIGHT, |mut header| {
                    header.col(|ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.add_space(13.0);
                            ui.add(
                                egui::Label::new(RichText::new("ROW").monospace().strong())
                                    .truncate(),
                            );
                        });
                    });
                    for (column, name) in document
                        .columns
                        .visible
                        .iter()
                        .copied()
                        .zip(document.headers.iter())
                    {
                        header.col(|ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                let width = ui.available_width();
                                ui.add_sized(
                                    [width, 13.0],
                                    egui::Label::new(
                                        RichText::new(column.saturating_add(1).to_string())
                                            .monospace()
                                            .size(11.0),
                                    )
                                    .halign(Align::Center)
                                    .truncate(),
                                );
                                if let Some(edit) = active_header_edit
                                    .as_mut()
                                    .filter(|edit| edit.column == column)
                                {
                                    let response = ui.add_sized(
                                        [width, 17.0],
                                        egui::TextEdit::singleline(&mut edit.draft)
                                            .id(header_edit_id(column))
                                            .margin(egui::Margin::ZERO),
                                    );
                                    let _ = ui.ctx().accesskit_node_builder(response.id, |node| {
                                        node.set_label(format!(
                                            "New name for file column {}",
                                            column.saturating_add(1)
                                        ));
                                    });
                                    if edit.focus_requested {
                                        response.request_focus();
                                        edit.focus_requested = false;
                                    }
                                    if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                                        cancel_header_edit = true;
                                    } else if (response.has_focus()
                                        && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                                        || response.lost_focus()
                                    {
                                        commit_header_edit = true;
                                    }
                                } else {
                                    let editable = document.header_is_editable(column);
                                    let response = ui.add_sized(
                                        [width, 17.0],
                                        egui::Label::new(RichText::new(name).strong())
                                            .truncate()
                                            .sense(if editable {
                                                egui::Sense::click()
                                            } else {
                                                egui::Sense::hover()
                                            }),
                                    );
                                    if editable {
                                        response.widget_info(|| {
                                            egui::WidgetInfo::labeled(
                                                egui::WidgetType::Button,
                                                ui.is_enabled(),
                                                format!(
                                                    "Rename file column {} ({name})",
                                                    column.saturating_add(1)
                                                ),
                                            )
                                        });
                                        let activate = response.clicked()
                                            || (response.has_focus()
                                                && ui.input(|input| {
                                                    input.key_pressed(egui::Key::Enter)
                                                }));
                                        response.on_hover_text("Click to rename");
                                        if activate {
                                            begin_header_edit = Some(column);
                                        }
                                    }
                                }
                            });
                        });
                    }
                })
                .body(|body| {
                    body.rows(ROW_HEIGHT, row_count, |mut table_row| {
                        let row_index = table_row.index();
                        let (record_row, fields) = document
                            .visible_row(row_index)
                            .expect("the table row is visible");
                        let display_row = record_row
                            .saturating_sub(document.data_start)
                            .saturating_add(1);
                        table_row.col(|ui| {
                            ui.scope_builder(
                                egui::UiBuilder::new().id(("row-selection", record_row)),
                                |ui| {
                                let selected = document
                                    .selection
                                    .is_some_and(|selection| selection.selects_row(record_row));
                                let color = if selected {
                                    ui.visuals().selection.stroke.color
                                } else {
                                    Color32::from_rgb(49, 85, 217)
                                };
                                let response = ui.add_sized(
                                    [ui.available_width(), ROW_HEIGHT],
                                    egui::Button::selectable(
                                        selected,
                                        RichText::new(display_row.to_string())
                                            .monospace()
                                            .color(color),
                                    )
                                    .small(),
                                );
                                let enabled = ui.is_enabled();
                                response.widget_info(|| {
                                    egui::WidgetInfo::selected(
                                        egui::WidgetType::SelectableLabel,
                                        enabled,
                                        selected,
                                        format!("Select row {display_row}"),
                                    )
                                });
                                if response.clicked() {
                                    response.request_focus();
                                    clicked_selection =
                                        Some(GridSelection::Row { row: record_row });
                                }
                                },
                            );
                        });
                        for (visible_column, column) in
                            document.columns.visible.iter().copied().enumerate()
                        {
                            table_row.col(|ui| {
                                ui.scope_builder(
                                    egui::UiBuilder::new()
                                        .id(("cell-selection", record_row, column)),
                                    |ui| {
                                    let text = fields
                                        .get(column)
                                        .map_or_else(String::new, |field| field_text(field));
                                    let selected = document.selection.is_some_and(|selection| {
                                        selection.selects_cell(record_row, column)
                                    });
                                    let response = ui.add_sized(
                                        [ui.available_width(), ROW_HEIGHT],
                                        egui::Button::selectable(
                                            selected,
                                            RichText::new(&text).monospace(),
                                        )
                                        .small(),
                                    );
                                    let enabled = ui.is_enabled();
                                    let header = &document.headers[visible_column];
                                    response.widget_info(|| {
                                        egui::WidgetInfo::selected(
                                            egui::WidgetType::SelectableLabel,
                                            enabled,
                                            selected,
                                            format!(
                                                "Select row {display_row}, column {} ({header}): {text}",
                                                column.saturating_add(1)
                                            ),
                                        )
                                    });
                                    if response.clicked() {
                                        response.request_focus();
                                        clicked_selection = Some(GridSelection::Cell {
                                            row: record_row,
                                            column,
                                        });
                                    }
                                    if reveal_cell == Some((record_row, column)) {
                                        response.scroll_to_me(Some(Align::Center));
                                    }
                                    },
                                );
                            });
                        }
                    });
                });
        });

    if cancel_header_edit {
        active_header_edit = None;
    } else if commit_header_edit && let Some(edit) = active_header_edit.take() {
        let _ = document.rename_header(edit.column, edit.draft);
    }
    if let Some(column) = begin_header_edit {
        if let Some(edit) = active_header_edit.take() {
            let _ = document.rename_header(edit.column, edit.draft);
        }
        document.begin_header_edit(column);
    } else {
        document.header_edit = active_header_edit;
    }
    clicked_selection
}

fn scrollbar_thumb_height(track_height: f32, total_rows: u64, visible_rows: usize) -> f32 {
    if total_rows == 0 {
        return track_height;
    }
    let ratio = (visible_rows as f64 / total_rows as f64).min(1.0) as f32;
    (track_height * ratio).clamp(MIN_THUMB_HEIGHT.min(track_height), track_height)
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(20.0, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(13.0, FontFamily::Monospace),
    );
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);

    let mut visuals = egui::Visuals::light();
    visuals.override_text_color = Some(Color32::from_rgb(24, 35, 42));
    visuals.selection.bg_fill = Color32::from_rgb(49, 85, 217);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, Color32::WHITE);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(238, 242, 244);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(220, 228, 233);
    visuals.widgets.active.bg_fill = Color32::from_rgb(204, 218, 227);
    visuals.faint_bg_color = Color32::from_rgb(235, 240, 242);
    visuals.extreme_bg_color = Color32::from_rgb(250, 251, 251);
    style.visuals = visuals;
    ctx.set_style(style);
}

fn panel_frame(fill: Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(200, 209, 213)))
}

fn field_text(field: &[u8]) -> String {
    let rendered = String::from_utf8_lossy(field)
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    if rendered.chars().count() <= 120 {
        rendered
    } else {
        rendered.chars().take(117).collect::<String>() + "..."
    }
}

fn selection_text(row: &Row, selection: GridSelection, max_bytes: usize) -> Result<String, String> {
    selection_fields_text(&row.fields, selection, max_bytes)
}

fn selection_fields_text(
    fields: &[Vec<u8>],
    selection: GridSelection,
    max_bytes: usize,
) -> Result<String, String> {
    match selection {
        GridSelection::Cell { column, .. } => {
            let mut output = String::new();
            append_clipboard_field(
                &mut output,
                fields.get(column).map_or(&[], Vec::as_slice),
                false,
                max_bytes,
            )?;
            Ok(output)
        }
        GridSelection::Row { .. } => {
            let mut output = String::new();
            for (index, field) in fields.iter().enumerate() {
                if index > 0 {
                    ensure_copy_capacity(&output, 1, max_bytes)?;
                    output.push('\t');
                }
                append_clipboard_field(&mut output, field, true, max_bytes)?;
            }
            Ok(output)
        }
    }
}

fn append_clipboard_field(
    output: &mut String,
    field: &[u8],
    quote_for_tsv: bool,
    max_bytes: usize,
) -> Result<(), String> {
    let text = std::str::from_utf8(field)
        .map_err(|_| "The selected data is not valid UTF-8 and cannot be copied yet.".to_owned())?;
    let needs_quotes = quote_for_tsv
        && text
            .bytes()
            .any(|byte| matches!(byte, b'\t' | b'\r' | b'\n' | b'"'));
    let added = text.len().saturating_add(if needs_quotes {
        2 + text.bytes().filter(|byte| *byte == b'"').count()
    } else {
        0
    });
    ensure_copy_capacity(output, added, max_bytes)?;

    if needs_quotes {
        output.push('"');
        for character in text.chars() {
            if character == '"' {
                output.push('"');
            }
            output.push(character);
        }
        output.push('"');
    } else {
        output.push_str(text);
    }
    Ok(())
}

fn ensure_copy_capacity(output: &str, added: usize, max_bytes: usize) -> Result<(), String> {
    if output.len().saturating_add(added) > max_bytes {
        Err(format!(
            "Copy exceeds the {} clipboard limit.",
            format_bytes(max_bytes as u64)
        ))
    } else {
        Ok(())
    }
}

fn display_delimiter(delimiter: u8) -> &'static str {
    match delimiter {
        b',' => "comma",
        b'\t' => "tab",
        b'|' => "pipe",
        b';' => "semicolon",
        _ => "custom",
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::Path;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use eframe::egui;

    use super::{
        Action, COLUMN_INPUT_ID, COLUMN_POSITION_INPUT_ID, ColumnCommand, ColumnView,
        DelimiterMode, Document, FIND_INPUT_ID, FilterOperator, FilterProgress, FilterQuery,
        GridSelection, HeaderMode, IndexConfig, OpenOptions, QuarryApp, Row, SearchProgress,
        Session, column_drop_position, column_window_controls, copy_control,
        filtered_export_controls, filtered_export_file_name, logical_viewport_start,
        max_viewport_start, page_controls, parse_column_position, parse_data_row,
        parse_file_column, row_for_scroll_fraction, save_as_file_name, scroll_fraction_for_row,
        search_controls, selection_text, show_column_manager, show_filter_manager, show_grid,
    };

    fn click_accessible_button(
        label: &str,
        mut render: impl FnMut(&mut egui::Ui) -> Option<Action>,
    ) -> Option<Action> {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut action = None;
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                action = render(ui);
            });
        });
        let target = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present")
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some(label)
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("{label} is not an accessible button"));

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    },
                )],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    action = render(ui);
                });
            },
        );
        action
    }

    fn click_page_control(label: &str) -> Option<Action> {
        click_accessible_button(label, page_controls)
    }

    fn grid_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 780.0),
            )),
            ..Default::default()
        }
    }

    fn grid_control_id(
        ctx: &egui::Context,
        label: &str,
        document: &mut Document,
    ) -> egui::accesskit::NodeId {
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, document).unwrap();
            });
        });
        output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present")
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some(label)
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("{label} is not an accessible button"))
    }

    fn click_grid_control(
        label: &str,
        document: &mut Document,
    ) -> (egui::Context, egui::accesskit::NodeId) {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let target = grid_control_id(&ctx, label, document);

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, document).unwrap();
                });
            },
        );
        (ctx, target)
    }

    fn click_column_manager_control(
        role: egui::accesskit::Role,
        label: &str,
        document: &Document,
        column_input: &mut String,
        column_position_input: &mut String,
    ) -> ColumnCommand {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut open = true;
        let mut command = None;
        let output = ctx.run(grid_input(), |ctx| {
            command = show_column_manager(
                ctx,
                &mut open,
                column_input,
                column_position_input,
                document,
            );
        });
        let target = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present")
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == role
                    && node.label() == Some(label)
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("{label} is not an accessible control"));

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                command = show_column_manager(
                    ctx,
                    &mut open,
                    column_input,
                    column_position_input,
                    document,
                );
            },
        );
        command.unwrap_or_else(|| panic!("{label} did not produce a column command"))
    }

    fn submit_column_manager_input(
        document: &Document,
        column_input: &mut String,
        column_position_input: &mut String,
        focused_input: &str,
    ) -> Option<ColumnCommand> {
        let ctx = egui::Context::default();
        let mut open = true;
        let mut command = None;
        let _ = ctx.run(grid_input(), |ctx| {
            command = show_column_manager(
                ctx,
                &mut open,
                column_input,
                column_position_input,
                document,
            );
        });
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new(focused_input)));
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..grid_input()
            },
            |ctx| {
                command = show_column_manager(
                    ctx,
                    &mut open,
                    column_input,
                    column_position_input,
                    document,
                );
            },
        );
        command
    }

    fn finish_index(document: &mut Document) {
        let job = document.job.take().expect("index job should be active");
        document.index = Some(job.wait().unwrap());
        document.progress.done = true;
        document.progress.bytes_scanned = document.session.file_size;
    }

    fn finish_search(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !document
            .search_job
            .as_ref()
            .expect("search job should be active")
            .progress()
            .done
        {
            assert!(Instant::now() < deadline, "search timed out");
            std::thread::yield_now();
        }
        document.poll_search().unwrap();
    }

    fn finish_filter(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            document.poll_filter().unwrap();
            if document.filter_job.is_none() && !document.filter_rows_loading() {
                break;
            }
            assert!(Instant::now() < deadline, "filter timed out");
            std::thread::yield_now();
        }
    }

    fn finish_filter_read(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while document.filter_rows_loading() {
            document.poll_filter().unwrap();
            assert!(Instant::now() < deadline, "filter read timed out");
            std::thread::yield_now();
        }
    }

    fn finish_filtered_export(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while document.export_job.is_some() {
            document.poll_filtered_export().unwrap();
            assert!(Instant::now() < deadline, "filtered export timed out");
            std::thread::yield_now();
        }
    }

    #[test]
    fn page_navigation_controls_are_clickable() {
        assert!(matches!(
            click_page_control("Page Up"),
            Some(Action::PageUp)
        ));
        assert!(matches!(
            click_page_control("Page Down"),
            Some(Action::PageDown)
        ));
        assert!(matches!(
            click_accessible_button("First columns", |ui| column_window_controls(ui, 8)),
            Some(Action::FirstColumns)
        ));
        assert!(matches!(
            click_accessible_button("Columns…", |ui| column_window_controls(ui, 0)),
            Some(Action::OpenColumns)
        ));
        assert!(matches!(
            click_accessible_button("Copy", |ui| copy_control(ui, true)),
            Some(Action::CopySelection)
        ));
    }

    #[test]
    fn filter_manager_applies_a_labelled_multiline_equals_value_by_button() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-a11y-{name}.csv"));
        fs::write(
            &path,
            b"name,note\nfirst,\"line one\nline two\"\nsecond,single line\n",
        )
        .unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        app.filters_open = true;
        app.filter_rules[0].column_input = "2".to_owned();
        app.filter_rules[0].operator = FilterOperator::Equals;

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut action = None;
        let output = ctx.run(grid_input(), |ctx| {
            action = show_filter_manager(
                ctx,
                &mut app.filters_open,
                &mut app.filter_rules,
                app.document.as_ref().unwrap(),
            );
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|(_, node)| {
                    matches!(
                        node.role(),
                        egui::accesskit::Role::TextInput
                            | egui::accesskit::Role::MultilineTextInput
                    ) && !node.labelled_by().is_empty()
                })
                .count(),
            2
        );
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::MultilineTextInput
                && !node.labelled_by().is_empty()
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::ComboBox && !node.labelled_by().is_empty()
        }));
        let target = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Apply filters")
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .expect("Apply filters should be accessible");

        ctx.memory_mut(|memory| {
            memory.request_focus(super::filter_value_input_id(0));
        });
        let _ = ctx.run(
            egui::RawInput {
                events: vec![
                    egui::Event::Text("line one".to_owned()),
                    egui::Event::Key {
                        key: egui::Key::Enter,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::Text("line two".to_owned()),
                ],
                ..grid_input()
            },
            |ctx| {
                action = show_filter_manager(
                    ctx,
                    &mut app.filters_open,
                    &mut app.filter_rules,
                    app.document.as_ref().unwrap(),
                );
            },
        );
        assert!(action.is_none(), "Enter should insert data, not apply");
        assert_eq!(app.filter_rules[0].value_input, "line one\nline two");

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                action = show_filter_manager(
                    ctx,
                    &mut app.filters_open,
                    &mut app.filter_rules,
                    app.document.as_ref().unwrap(),
                );
            },
        );
        assert!(matches!(action, Some(Action::ApplyFilter)));

        app.apply(&ctx, action.unwrap());
        let document = app.document.as_mut().unwrap();
        finish_filter(document);
        assert_eq!(document.available_filter_rows(), 1);
        assert_eq!(document.visible_filter_rows()[0].fields[0], b"first");
        assert_eq!(
            document.visible_filter_rows()[0].fields[1],
            b"line one\nline two"
        );
        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filter_manager_adds_and_removes_accessible_rules_and_suppresses_copy() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-rules-a11y-{name}.csv"));
        fs::write(&path, b"name,status\nfirst,keep\n").unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut open = true;
        let mut rules = vec![super::FilterRuleDraft::default()];

        let output = ctx.run(grid_input(), |ctx| {
            let _ = show_filter_manager(ctx, &mut open, &mut rules, &document);
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button
                && node.label() == Some("Remove rule 1")
                && node.is_disabled()
        }));
        let add_target = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Add AND rule")
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .expect("Add AND rule should be accessible");

        ctx.memory_mut(|memory| memory.request_focus(super::filter_value_input_id(0)));
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: add_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                let _ = show_filter_manager(ctx, &mut open, &mut rules, &document);
            },
        );
        assert_eq!(rules.len(), 2);
        assert!(!ctx.memory(|memory| {
            memory
                .focused()
                .is_some_and(|focused| super::is_filter_text_input(focused, rules.len()))
        }));
        rules[0].value_input = "first".into();
        rules[1].value_input = "second".into();

        let output = ctx.run(grid_input(), |ctx| {
            let _ = show_filter_manager(ctx, &mut open, &mut rules, &document);
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|(_, node)| {
                    matches!(
                        node.role(),
                        egui::accesskit::Role::TextInput
                            | egui::accesskit::Role::MultilineTextInput
                    ) && !node.labelled_by().is_empty()
                })
                .count(),
            4
        );
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|(_, node)| {
                    node.role() == egui::accesskit::Role::ComboBox && !node.labelled_by().is_empty()
                })
                .count(),
            2
        );
        let remove_target = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Remove rule 2")
                    && node.supports_action(egui::accesskit::Action::Click)
            })
            .map(|(id, _)| *id)
            .expect("Remove rule 2 should be enabled with two rules");

        for focused in [
            super::filter_column_input_id(1),
            super::filter_value_input_id(1),
        ] {
            ctx.memory_mut(|memory| memory.request_focus(focused));
            let mut selection_copy = true;
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![egui::Event::Copy],
                    ..grid_input()
                },
                |ctx| {
                    selection_copy = super::selection_copy_requested(ctx, rules.len(), None);
                },
            );
            assert!(!selection_copy);
        }

        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::AccessKitActionRequest(
                    egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target: remove_target,
                        data: None,
                    },
                )],
                ..grid_input()
            },
            |ctx| {
                let _ = show_filter_manager(ctx, &mut open, &mut rules, &document);
            },
        );
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].value_input, "first");
        assert!(!ctx.memory(|memory| {
            memory
                .focused()
                .is_some_and(|focused| super::is_filter_text_input(focused, 2))
        }));

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn applying_two_filter_rules_shows_only_rows_matching_both() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-and-{name}.csv"));
        fs::write(
            &path,
            b"id,status,region\n1,keep,east\n2,keep,west\n3,skip,east\n4,keep,east\n",
        )
        .unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(path.clone()).unwrap();
        app.filter_rules = vec![
            super::FilterRuleDraft {
                column_input: "2".into(),
                operator: FilterOperator::Equals,
                value_input: "keep".into(),
            },
            super::FilterRuleDraft {
                column_input: "3".into(),
                operator: FilterOperator::Equals,
                value_input: "east".into(),
            },
        ];
        let ctx = egui::Context::default();
        app.apply(&ctx, Action::ApplyFilter);
        assert!(app.notice.is_none());

        let document = app.document.as_mut().unwrap();
        finish_filter(document);
        assert_eq!(document.filter_query.as_ref().unwrap().predicates.len(), 2);
        assert_eq!(document.available_filter_rows(), 2);
        assert_eq!(
            document
                .visible_filter_rows()
                .iter()
                .map(|row| row.fields[0].as_slice())
                .collect::<Vec<_>>(),
            vec![b"1".as_slice(), b"4".as_slice()]
        );
        assert!(document.visible_filter_rows().iter().all(|row| {
            row.fields[1].as_slice() == b"keep" && row.fields[2].as_slice() == b"east"
        }));

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filtered_export_control_is_accessible_only_for_a_completed_active_filter() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-export-control-{name}.csv"));
        fs::write(&path, b"name,status\nfirst,keep\nsecond,skip\n").unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                assert!(filtered_export_controls(ui, &document).is_none());
            });
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(!tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button
                && node.label() == Some("Export Filtered Rows…")
        }));

        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);
        assert!(document.is_filtered_export_ready());
        assert!(matches!(
            click_accessible_button("Export Filtered Rows…", |ui| {
                filtered_export_controls(ui, &document)
            }),
            Some(Action::ChooseFilteredExport)
        ));

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn header_renames_keep_lossless_source_identity_and_clear_when_restored() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-header-rename-{name}.csv"));
        let long_header = "x".repeat(140);
        let source = format!("\"line\nname\",,{long_header}\nfirst,second,third\n");
        fs::write(&path, source.as_bytes()).unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();

        assert_eq!(document.source_header_name(0), Some("line\nname"));
        assert_eq!(document.source_header_name(1), Some(""));
        assert_eq!(document.source_header_name(2), Some(long_header.as_str()));
        assert!(document.column_name(2).ends_with("..."));

        document.begin_header_edit(0);
        assert_eq!(
            document
                .header_edit
                .as_ref()
                .map(|edit| edit.draft.as_str()),
            Some("line\nname")
        );
        document.header_edit.as_mut().unwrap().draft = "renamed".into();
        document.move_column(0, 2).unwrap();
        assert!(document.is_dirty());
        assert_eq!(document.column_name(0), "renamed");

        document.set_column_shown(0, false).unwrap();
        document.set_column_shown(0, true).unwrap();
        assert_eq!(document.column_name(0), "renamed");
        assert_eq!(
            document.header_renames.get(&0).map(String::as_str),
            Some("renamed")
        );

        document.rename_header(0, "line\nname".into()).unwrap();
        assert!(!document.is_dirty());
        document.rename_header(1, "Column 2".into()).unwrap();
        assert!(document.is_dirty());
        document.rename_header(1, String::new()).unwrap();
        assert!(!document.is_dirty());
        assert_eq!(fs::read(&path).unwrap(), source.as_bytes());

        document.shutdown();
        let mut no_header = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert!(!no_header.header_is_editable(0));
        no_header.begin_header_edit(0);
        assert!(no_header.header_edit.is_none());
        assert_eq!(
            no_header.rename_header(0, "renamed".into()).unwrap_err(),
            "Only columns in the source header row can be renamed."
        );
        no_header.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn accessible_header_editor_commits_cancels_and_keeps_copy_in_the_editor() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-header-editor-{name}.csv"));
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        document.selection = Some(GridSelection::Cell { row: 1, column: 0 });

        let (ctx, _) = click_grid_control("Rename file column 1 (name)", &mut document);
        assert_eq!(document.header_edit.as_ref().unwrap().column, 0);
        document.header_edit.as_mut().unwrap().draft = "renamed".into();
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::TextInput
                && node.label() == Some("New name for file column 1")
        }));
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert!(document.header_edit.is_none());
        assert_eq!(document.column_name(0), "renamed");

        let _ = click_grid_control("Rename file column 1 (renamed)", &mut document);
        document.header_edit.as_mut().unwrap().draft = "cancelled".into();
        let _ = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert!(document.header_edit.is_none());
        assert_eq!(document.column_name(0), "renamed");

        document.begin_header_edit(0);
        let mut selection_copy = true;
        let _ = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..grid_input()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
                selection_copy = super::selection_copy_requested(ctx, 0, Some(0));
            },
        );
        assert!(!selection_copy);

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dirty_lifecycle_blocks_loss_and_save_as_reopens_the_edited_copy() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("quarry-save-as-ui-{name}.csv"));
        let other = std::env::temp_dir().join(format!("quarry-save-as-ui-{name}-other.csv"));
        let unopenable = std::env::temp_dir().join(format!("quarry-save-as-ui-{name}-removed.csv"));
        let destination = std::env::temp_dir().join(format!("quarry-save-as-ui-{name}-edited.csv"));
        let source_bytes = b"name,value\nfirst,1\n";
        fs::write(&source, source_bytes).unwrap();
        fs::write(&other, b"other,value\nsecond,2\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(source.clone()).unwrap();
        let document = app.document.as_mut().unwrap();
        document.rename_header(0, "renamed".into()).unwrap();
        let ctx = egui::Context::default();
        let mut close_input = grid_input();
        close_input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let close_output = ctx.run(close_input, |ctx| app.intercept_dirty_close(ctx));
        assert!(app.close_confirmation_open);
        assert!(
            close_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
        );
        app.close_after_save = true;
        app.keep_editing();
        assert!(!app.close_confirmation_open);
        assert!(!app.close_after_save);
        assert_eq!(
            app.open_path(other.clone()).unwrap_err(),
            "Discard or save your changes before opening another file."
        );
        app.handle_dropped_paths(vec![Some(other.clone())]);
        assert_eq!(app.document.as_ref().unwrap().session.path(), source);
        assert_eq!(
            app.document
                .as_mut()
                .unwrap()
                .start_filtered_export(destination.clone())
                .unwrap_err(),
            "Save or discard your changes before exporting filtered rows."
        );

        app.document
            .as_mut()
            .unwrap()
            .start_save_as(unopenable.clone())
            .unwrap();
        let mut saving_close_input = grid_input();
        saving_close_input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let saving_close_output = ctx.run(saving_close_input, |ctx| app.intercept_dirty_close(ctx));
        assert!(app.close_after_save);
        assert!(!app.close_confirmation_open);
        assert!(
            saving_close_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
        );
        let document = app.document.as_mut().unwrap();
        assert_eq!(
            document.rename_header(1, "amount".into()).unwrap_err(),
            "Wait for Save As to finish before editing headers."
        );
        document.discard_header_edits();
        assert!(document.is_dirty());
        assert_eq!(
            app.open_path(other.clone()).unwrap_err(),
            "Cancel the active Save As before opening another file."
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .document
            .as_ref()
            .unwrap()
            .save_as_job
            .as_ref()
            .unwrap()
            .progress()
            .done
        {
            assert!(Instant::now() < deadline, "Save As timed out");
            std::thread::yield_now();
        }
        fs::remove_file(&unopenable).unwrap();
        let mut frame = eframe::Frame::_new_kittest();
        let _ = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(app.close_confirmation_open);
        assert!(!app.close_after_save);
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), source);
        assert!(document.is_dirty());
        assert!(document.save_as_status.is_none());

        app.close_confirmation_open = false;
        app.document
            .as_mut()
            .unwrap()
            .start_save_as(destination.clone())
            .unwrap();
        app.close_after_save = true;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .document
            .as_ref()
            .unwrap()
            .save_as_job
            .as_ref()
            .unwrap()
            .progress()
            .done
        {
            assert!(Instant::now() < deadline, "Save As timed out");
            std::thread::yield_now();
        }
        let saved_output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        assert!(
            saved_output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .unwrap()
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::Close))
        );
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), destination);
        assert_eq!(document.column_name(0), "renamed");
        assert!(!document.is_dirty());
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&destination).unwrap(), b"renamed,value\nfirst,1\n");
        assert_eq!(
            save_as_file_name(Path::new("report.csv")),
            "report-edited.csv"
        );

        app.document.as_mut().unwrap().shutdown();
        for path in [source, other, destination] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn filtered_export_reports_success_and_never_overwrites() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-export-success-{name}.csv"));
        let destination =
            std::env::temp_dir().join(format!("quarry-export-success-{name}-filtered.csv"));
        let source = b"id,note,status\n1,\"line one\nline two\",keep\n2,skip,skip\n3,\"quote \"\"yes\"\"\",keep\n";
        let expected =
            b"id,note,status\n1,\"line one\nline two\",keep\n3,\"quote \"\"yes\"\"\",keep\n";
        fs::write(&path, source).unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let query = FilterQuery::single(2, FilterOperator::Equals, b"keep".to_vec());
        document.start_filter(query.clone()).unwrap();
        finish_filter(&mut document);

        document.start_filtered_export(destination.clone()).unwrap();
        assert_eq!(
            document.rename_header(0, "renamed".into()).unwrap_err(),
            "Wait for the filtered export to finish before editing headers."
        );
        assert_eq!(
            document.clear_filter().unwrap_err(),
            "Cancel the active export before clearing filters."
        );
        assert_eq!(
            document.start_filter(query.clone()).unwrap_err(),
            "Cancel the active export before changing filters."
        );
        finish_filtered_export(&mut document);

        assert_eq!(fs::read(&destination).unwrap(), expected);
        let status = document.export_status.as_deref().unwrap();
        assert!(status.contains("2 rows"));
        assert!(status.contains(&destination.display().to_string()));
        assert_eq!(
            document
                .start_filtered_export(destination.clone())
                .unwrap_err(),
            "export destination already exists"
        );
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert_eq!(
            document.start_filtered_export(path.clone()).unwrap_err(),
            "export destination must differ from the source file"
        );
        assert_eq!(fs::read(&path).unwrap(), source);

        document.start_filter(query).unwrap();
        assert!(document.export_progress.is_none());
        assert!(document.export_status.is_none());
        finish_filter(&mut document);
        document.shutdown();
        for path in [path, destination] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn filtered_export_cancel_reopen_and_picker_cancel_are_safe() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("quarry-export-life-{name}-first.csv"));
        let second = std::env::temp_dir().join(format!("quarry-export-life-{name}-second.csv"));
        let cancelled =
            std::env::temp_dir().join(format!("quarry-export-life-{name}-cancelled.csv"));
        let reopened = std::env::temp_dir().join(format!("quarry-export-life-{name}-reopened.csv"));
        let mut file = File::create(&first).unwrap();
        file.write_all(b"name,status\n").unwrap();
        file.write_all(&b"row,keep\n".repeat(1_000_000)).unwrap();
        drop(file);
        fs::write(&second, b"name,status\nsecond,keep\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(first.clone()).unwrap();
        let document = app.document.as_mut().unwrap();
        let file_size = document.session.file_size;
        document.filter_query = Some(FilterQuery::single(
            1,
            FilterOperator::Equals,
            b"keep".to_vec(),
        ));
        document.filter_progress = Some(FilterProgress {
            bytes_scanned: file_size,
            rows_scanned: 1_000_001,
            matches_found: 1_000_000,
            file_size,
            elapsed: Duration::from_millis(1),
            done: true,
            cancelled: false,
        });

        app.notice = Some("unchanged".into());
        app.export_picker_result(None);
        assert_eq!(app.notice.as_deref(), Some("unchanged"));
        app.export_picker_result(Some(first.clone()));
        assert_eq!(
            app.notice.as_deref(),
            Some("export destination must differ from the source file")
        );
        app.export_picker_result(Some(cancelled.clone()));
        let document = app.document.as_mut().unwrap();
        document.cancel_filtered_export();
        assert!(document.export_cancel_requested);
        assert_eq!(
            document.export_status.as_deref(),
            Some("Cancelling export… Wait for it to finish before opening or reapplying the file.")
        );
        finish_filtered_export(document);
        assert_eq!(
            document.export_status.as_deref(),
            Some("Export cancelled. No output file was created.")
        );
        assert!(!cancelled.exists());

        document.start_filtered_export(reopened.clone()).unwrap();
        let error = app.open_path(second.clone()).unwrap_err();
        assert_eq!(
            error,
            "Cancel the active export and wait for it to finish before opening another file."
        );
        let document = app.document.as_mut().unwrap();
        assert_eq!(document.session.path(), first);
        assert!(document.export_job.is_some());

        document.cancel_filtered_export();
        finish_filtered_export(document);
        app.open_path(second.clone()).unwrap();
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), second);
        assert!(document.export_job.is_none());
        assert!(document.export_status.is_none());
        assert_eq!(
            filtered_export_file_name(Path::new("report.csv")),
            "report-filtered.csv"
        );
        assert_eq!(
            filtered_export_file_name(Path::new("report")),
            "report-filtered"
        );

        app.document.as_mut().unwrap().shutdown();
        for path in [first, second, reopened] {
            if path.exists() {
                fs::remove_file(path).unwrap();
            }
        }
    }

    #[test]
    fn filtered_grid_is_match_only_bounded_and_keeps_source_column_identity() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-grid-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "id,status,note").unwrap();
        for row in 1..=200 {
            let status = if row % 3 == 0 { "keep" } else { "skip" };
            writeln!(file, "{row},{status},note{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.move_column(1, 0).unwrap();
        document.set_column_shown(1, false).unwrap();
        let order = document.columns.order.clone();
        let hidden = document.columns.hidden.clone();

        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);
        document.set_visible_rows(7).unwrap();

        assert_eq!(document.available_filter_rows(), 66);
        assert_eq!(document.filter_viewport_start, 0);
        assert_eq!(document.visible_row_count(), 7);
        assert!(
            document
                .visible_filter_rows()
                .iter()
                .all(|row| { row.fields.get(1).map(Vec::as_slice) == Some(b"keep".as_slice()) })
        );
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, 0);
        assert_eq!(document.visible_filter_rows()[0].row, 3);
        assert_eq!(document.columns.order, order);
        assert_eq!(document.columns.hidden, hidden);

        let first_row = document.visible_filter_rows()[0].row;
        document.selection = Some(GridSelection::Cell {
            row: first_row,
            column: 2,
        });
        assert_eq!(document.copy_selection_text().unwrap(), "note3");
        document.selection = Some(GridSelection::Row { row: first_row });
        assert_eq!(document.copy_selection_text().unwrap(), "3\tkeep\tnote3");

        document.page(1).unwrap();
        assert_eq!(document.filter_viewport_start, 7);
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, 7);
        assert_eq!(document.visible_filter_rows()[0].row, 24);
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * super::OVERSCAN_ROWS);

        let final_start =
            max_viewport_start(document.available_filter_rows(), document.visible_rows);
        document.navigate_filter(final_start).unwrap();
        assert!(document.filter_rows_loading());
        finish_filter_read(&mut document);
        assert_eq!(document.visible_filter_rows().last().unwrap().row, 198);
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * super::OVERSCAN_ROWS);
        document.page(-1).unwrap();
        assert_eq!(
            document.filter_viewport_start,
            final_start.saturating_sub(document.visible_rows as u64)
        );

        document.clear_filter().unwrap();
        assert!(!document.filter_active());
        assert!(document.filtered_rows.is_empty());
        assert_eq!(document.columns.order, order);
        assert_eq!(document.columns.hidden, hidden);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn active_filter_navigation_refreshes_a_stale_snapshot_before_reading() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-stale-{name}.csv"));
        let prefix_path =
            std::env::temp_dir().join(format!("quarry-filter-stale-prefix-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        file.write_all(b"name,status\n").unwrap();
        file.write_all(&b"row,keep\n".repeat(2_000)).unwrap();
        drop(file);
        let mut prefix = File::create(&prefix_path).unwrap();
        prefix.write_all(b"name,status\n").unwrap();
        prefix.write_all(&b"row,keep\n".repeat(1_000)).unwrap();
        drop(prefix);

        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        document.visible_rows = 5;
        let query = FilterQuery::single(1, FilterOperator::Equals, b"keep".to_vec());
        let stale_session = Session::open(
            &prefix_path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let stale_job = stale_session.start_filter(query.clone()).unwrap();
        let stale_index = stale_job.wait().unwrap();
        let stale_matches = stale_index.matches_found();
        assert_eq!(stale_matches, 1_000);

        document.start_filter(query).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let progress = loop {
            let progress = document.filter_job.as_ref().unwrap().progress();
            if progress.done {
                break progress;
            }
            assert!(Instant::now() < deadline, "filter did not complete");
            std::thread::yield_now();
        };
        assert_eq!(progress.matches_found, 2_000);
        document.filter_progress = Some(progress);
        document.filter_index = Some(stale_index);
        let target = max_viewport_start(progress.matches_found, document.visible_rows);
        let required = target
            .saturating_add(document.visible_rows as u64)
            .min(progress.matches_found);
        assert!(stale_matches < required);

        document.navigate_filter(target).unwrap();
        assert!(document.filter_rows_loading());
        finish_filter_read(&mut document);

        assert!(document.filter_index.as_ref().unwrap().matches_found() >= required);
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, target);
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * super::OVERSCAN_ROWS);
        document.shutdown();
        for path in [path, prefix_path] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn filter_navigation_is_async_and_latest_request_wins() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-latest-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "id,status").unwrap();
        for row in 0..25_000 {
            let status = if row % 20 == 0 { "keep" } else { "skip" };
            writeln!(file, "{row},{status}").unwrap();
        }
        drop(file);

        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        document.visible_rows = 5;
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);

        document.navigate_filter(100).unwrap();
        assert!(document.filter_read.is_some());
        assert!(document.visible_filter_rows().is_empty());
        assert_eq!(
            document.filter_empty_message(document.visible_row_count()),
            Some("Loading matching rows…")
        );

        document.navigate_filter(500).unwrap();
        assert!(document.filter_read.as_ref().unwrap().cancel_requested);
        document.navigate_filter(900).unwrap();
        assert_eq!(
            document.pending_filter_read,
            document.filter_read_window(900)
        );
        assert_eq!(document.filter_viewport_start, 900);

        finish_filter_read(&mut document);
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, 900);
        assert_eq!(document.visible_filter_rows()[0].fields[0], b"18000");
        assert!(
            document
                .visible_filter_rows()
                .iter()
                .all(|row| row.fields[1] == b"keep")
        );
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * super::OVERSCAN_ROWS);
        assert!(document.filter_read.is_none());
        assert!(document.pending_filter_read.is_none());

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filter_cancel_clear_and_reopen_reset_the_filter_lifecycle() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("quarry-filter-life-{name}-first.csv"));
        let second = std::env::temp_dir().join(format!("quarry-filter-life-{name}-second.csv"));
        let mut file = File::create(&first).unwrap();
        writeln!(file, "name,status").unwrap();
        for row in 0..50_000 {
            writeln!(file, "row{row},keep").unwrap();
        }
        drop(file);
        fs::write(&second, b"name,status\nsecond,keep\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.header_mode = HeaderMode::FirstRow;
        app.open_path(first.clone()).unwrap();
        let document = app.document.as_mut().unwrap();
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Contains,
                b"keep".to_vec(),
            ))
            .unwrap();
        document.cancel_filter();
        finish_filter(document);
        assert!(document.filter_active());
        let status = document.filter_status.as_deref().unwrap();
        assert!(
            status.starts_with("Filter cancelled after") || status.starts_with("Filter complete.")
        );

        document.clear_filter().unwrap();
        assert!(!document.filter_active());
        assert!(document.filter_job.is_none());
        assert!(document.filter_index.is_none());

        let query = FilterQuery::single(1, FilterOperator::Equals, b"keep".to_vec());
        document.start_filter(query.clone()).unwrap();
        finish_filter(document);
        document.navigate_filter(100).unwrap();
        document.navigate_filter(200).unwrap();
        assert!(document.filter_read.is_some());
        assert!(document.pending_filter_read.is_some());

        document.start_filter(query.clone()).unwrap();
        assert!(document.filter_read.is_none());
        assert!(document.pending_filter_read.is_none());
        finish_filter(document);
        document.navigate_filter(100).unwrap();
        document.navigate_filter(200).unwrap();
        assert!(document.filter_read.is_some());
        assert!(document.pending_filter_read.is_some());
        document.clear_filter().unwrap();
        assert!(document.filter_read.is_none());
        assert!(document.pending_filter_read.is_none());

        document.start_filter(query).unwrap();
        finish_filter(document);
        document.navigate_filter(100).unwrap();
        document.navigate_filter(200).unwrap();
        assert!(document.filter_read.is_some());
        assert!(document.pending_filter_read.is_some());
        app.filters_open = true;
        app.filter_rules[0].column_input = "2".into();
        app.filter_rules[0].value_input = "keep".into();
        app.filter_rules.push(super::FilterRuleDraft::default());
        app.open_path(second.clone()).unwrap();
        assert!(!app.filters_open);
        assert_eq!(app.filter_rules, vec![super::FilterRuleDraft::default()]);
        assert!(!app.document.as_ref().unwrap().filter_active());
        assert!(app.document.as_ref().unwrap().filter_read.is_none());
        assert!(app.document.as_ref().unwrap().pending_filter_read.is_none());

        app.document.as_mut().unwrap().shutdown();
        for path in [first, second] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn column_view_hides_reorders_resets_and_stays_bounded() {
        let mut view = ColumnView::new(40);
        assert_eq!(view.visible, (0..32).collect::<Vec<_>>());

        assert!(view.set_shown(5, false));
        assert!(!view.visible.contains(&5));
        assert_eq!(view.shown_count(), 39);
        assert!(view.move_column(39, 0));
        assert_eq!(&view.order[..3], &[39, 0, 1]);
        assert!(view.move_column(39, 39));
        assert_eq!(view.order, (0..40).collect::<Vec<_>>());
        assert!(view.move_column(0, 39));
        assert_eq!(&view.order[37..], &[38, 39, 0]);
        assert!(view.move_column(0, 0));
        assert_eq!(view.order, (0..40).collect::<Vec<_>>());
        assert!(view.move_column(39, 39));
        assert!(!view.move_column(40, 0));
        assert!(!view.move_column(39, 40));
        assert!(view.move_column(39, 0));

        assert_eq!(column_drop_position(0, 3, 4), 2);
        assert_eq!(column_drop_position(3, 1, 4), 1);
        assert_eq!(column_drop_position(2, 2, 4), 2);
        assert_eq!(column_drop_position(0, 4, 4), 3);

        assert!(view.view(39));
        assert!(view.visible.contains(&39));
        assert!(view.visible.len() <= super::MAX_VISIBLE_COLUMNS);

        for column in 0..40 {
            assert!(view.set_shown(column, false));
        }
        assert!(view.visible.is_empty());
        assert_eq!(view.start, 0);

        view.extend_to(42);
        assert_eq!(&view.order[..3], &[39, 0, 1]);
        assert_eq!(&view.order[39..], &[38, 40, 41]);
        assert_eq!(view.visible, [40, 41]);

        view.reset();
        assert_eq!(view.order, (0..42).collect::<Vec<_>>());
        assert!(view.hidden.iter().all(|hidden| !hidden));
        assert_eq!(view.visible, (0..32).collect::<Vec<_>>());
    }

    #[test]
    fn column_manager_exposes_labelled_keyboard_controls() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-column-a11y-{name}.csv"));
        let headers = (1..=40)
            .map(|column| format!("c{column}"))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(&path, format!("{headers}\n{}\n", vec!["x"; 40].join(","))).unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(document);
        app.column_input = "40".into();
        app.column_position_input = "1".into();

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut open = true;
        let mut input = app.column_input.clone();
        let mut position_input = app.column_position_input.clone();
        let mut command = None;
        let output = ctx.run(grid_input(), |ctx| {
            command = show_column_manager(
                ctx,
                &mut open,
                &mut input,
                &mut position_input,
                app.document.as_ref().unwrap(),
            );
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|(_, node)| {
                    node.role() == egui::accesskit::Role::TextInput
                        && !node.labelled_by().is_empty()
                })
                .count(),
            2
        );
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::CheckBox
                && node.label().is_some_and(|label| label.contains("c1"))
        }));
        assert!(
            tree.nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Drag") && node.is_hidden())
        );
        assert!(
            !tree
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Drag") && !node.is_hidden())
        );
        let command = click_column_manager_control(
            egui::accesskit::Role::Button,
            "Hide selected file column",
            app.document.as_ref().unwrap(),
            &mut app.column_input,
            &mut app.column_position_input,
        );
        app.apply_column_command(command);
        assert!(app.document.as_ref().unwrap().columns.hidden[39]);

        let command = click_column_manager_control(
            egui::accesskit::Role::Button,
            "View selected file column",
            app.document.as_ref().unwrap(),
            &mut app.column_input,
            &mut app.column_position_input,
        );
        app.apply_column_command(command);
        assert!(app.document.as_ref().unwrap().columns.visible.contains(&39));

        let command = click_column_manager_control(
            egui::accesskit::Role::Button,
            "Move selected file column to display position",
            app.document.as_ref().unwrap(),
            &mut app.column_input,
            &mut app.column_position_input,
        );
        app.apply_column_command(command);
        assert_eq!(
            &app.document.as_ref().unwrap().columns.order[..3],
            &[39, 0, 1]
        );

        app.column_input = "1".into();
        let command = click_column_manager_control(
            egui::accesskit::Role::CheckBox,
            "1  c1",
            app.document.as_ref().unwrap(),
            &mut app.column_input,
            &mut app.column_position_input,
        );
        app.apply_column_command(command);
        assert!(app.document.as_ref().unwrap().columns.hidden[0]);

        let command = click_column_manager_control(
            egui::accesskit::Role::Button,
            "Reset columns",
            app.document.as_ref().unwrap(),
            &mut app.column_input,
            &mut app.column_position_input,
        );
        app.apply_column_command(command);
        assert_eq!(
            app.document.as_ref().unwrap().columns.order,
            (0..40).collect::<Vec<_>>()
        );
        assert!(
            app.document
                .as_ref()
                .unwrap()
                .columns
                .hidden
                .iter()
                .all(|hidden| !hidden)
        );

        app.column_input = "40".into();
        let command = submit_column_manager_input(
            app.document.as_ref().unwrap(),
            &mut app.column_input,
            &mut app.column_position_input,
            COLUMN_INPUT_ID,
        );
        assert_eq!(command, Some(ColumnCommand::ViewInput));

        app.column_position_input = "2".into();
        let command = submit_column_manager_input(
            app.document.as_ref().unwrap(),
            &mut app.column_input,
            &mut app.column_position_input,
            COLUMN_POSITION_INPUT_ID,
        );
        assert_eq!(command, Some(ColumnCommand::MoveInput));
        app.apply_column_command(command.unwrap());
        assert_eq!(
            &app.document.as_ref().unwrap().columns.order[..3],
            &[0, 39, 1]
        );

        let order = app.document.as_ref().unwrap().columns.order.clone();
        app.column_position_input = "0".into();
        app.apply_column_command(ColumnCommand::MoveInput);
        assert_eq!(app.document.as_ref().unwrap().columns.order, order);
        assert_eq!(app.notice.as_deref(), Some("Display positions start at 1."));

        app.apply_column_command(ColumnCommand::Move {
            column: 39,
            position: 0,
        });
        assert_eq!(
            &app.document.as_ref().unwrap().columns.order[..3],
            &[39, 0, 1]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn selected_cells_and_rows_copy_full_bounded_text() {
        let row = Row {
            offset: 0,
            fields: vec![
                vec![b'x'; 200],
                b"has\ttab".to_vec(),
                b"say \"hi\"".to_vec(),
                Vec::new(),
            ],
        };
        let cell = GridSelection::Cell { row: 0, column: 0 };
        assert_eq!(selection_text(&row, cell, 200).unwrap(), "x".repeat(200));
        assert!(selection_text(&row, cell, 199).is_err());

        let selected_row = GridSelection::Row { row: 0 };
        assert_eq!(
            selection_text(&row, selected_row, 1024).unwrap(),
            format!("{}\t\"has\ttab\"\t\"say \"\"hi\"\"\"\t", "x".repeat(200))
        );

        let invalid = Row {
            offset: 0,
            fields: vec![vec![0xff]],
        };
        assert!(selection_text(&invalid, cell, 1024).is_err());
    }

    #[test]
    fn grid_selection_is_clickable_and_command_c_copies_decoded_data() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-copy-{name}.csv"));
        fs::write(
            &path,
            b"name,notes\nalpha,\"line one\nline two\"\nbeta,other\ngamma,third\ndelta,fourth\n",
        )
        .unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(document);
        let mut frame = eframe::Frame::_new_kittest();
        let output = ctx.run(grid_input(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame);
        });
        let target = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present")
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Select row 1, column 2 (notes): line one\\nline two")
            })
            .map(|(id, _)| *id)
            .expect("cell selection should be accessible");
        let output = ctx.run(
            egui::RawInput {
                events: vec![
                    egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                        action: egui::accesskit::Action::Click,
                        target,
                        data: None,
                    }),
                    egui::Event::Copy,
                ],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        let selection = app.document.as_ref().unwrap().selection.unwrap();
        assert!(matches!(
            selection,
            GridSelection::Cell {
                row: 1,
                column: 1,
                ..
            }
        ));
        assert_eq!(
            app.document
                .as_ref()
                .unwrap()
                .copy_selection_text()
                .unwrap(),
            "line one\nline two"
        );
        assert!(output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "line one\nline two")
        }));

        ctx.memory_mut(|memory| {
            memory.request_focus(egui::Id::new(FIND_INPUT_ID));
        });
        let output = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(!output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "line one\nline two")
        }));

        ctx.memory_mut(|memory| {
            memory.request_focus(egui::Id::new(COLUMN_INPUT_ID));
        });
        let output = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(!output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "line one\nline two")
        }));

        ctx.memory_mut(|memory| {
            memory.request_focus(egui::Id::new(COLUMN_POSITION_INPUT_ID));
        });
        let output = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..grid_input()
            },
            |ctx| {
                eframe::App::update(&mut app, ctx, &mut frame);
            },
        );
        assert!(!output.platform_output.commands.iter().any(|command| {
            matches!(command, egui::OutputCommand::CopyText(text) if text == "line one\nline two")
        }));

        let mut document = app.document.take().unwrap();

        let _ = click_grid_control("Select row 1", &mut document);
        assert!(matches!(
            document.selection,
            Some(GridSelection::Row { row: 1, .. })
        ));
        assert_eq!(
            document.copy_selection_text().unwrap(),
            "alpha\t\"line one\nline two\""
        );

        document.set_visible_rows(3).unwrap();
        let (ctx, row_id) = click_grid_control("Select row 3", &mut document);
        document.navigate(2).unwrap();
        assert_eq!(grid_control_id(&ctx, "Select row 3", &mut document), row_id);
        assert!(matches!(
            document.selection,
            Some(GridSelection::Row { row: 3 })
        ));
        document.set_visible_rows(1).unwrap();
        assert!(document.selection.is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reference_window_is_dense_and_visible_rows_adapt_to_height() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-density-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "name,value").unwrap();
        for row in 1..=120 {
            writeln!(file, "row{row},{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        super::configure_style(&ctx);
        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(document);
        let mut frame = eframe::Frame::_new_kittest();
        let render = |height: f32, app: &mut QuarryApp, frame: &mut eframe::Frame| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1728.0, height),
                    )),
                    ..Default::default()
                },
                |ctx| eframe::App::update(app, ctx, frame),
            )
        };
        let output = render(1052.0, &mut app, &mut frame);

        let document = app.document.as_ref().unwrap();
        let reference_rows = document.visible_rows;
        assert!(
            reference_rows >= 40,
            "maximized reference window fits only {reference_rows} rows"
        );
        assert!(document.display_end() >= 40);
        assert!(
            document.buffered_rows.len()
                <= document
                    .visible_rows
                    .saturating_add(2 * super::OVERSCAN_ROWS)
        );
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button && node.label() == Some("Select row 40")
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button
                && node
                    .label()
                    .is_some_and(|label| label.starts_with("Select row 40, column 1"))
        }));

        let _ = render(852.0, &mut app, &mut frame);
        let smaller_rows = app.document.as_ref().unwrap().visible_rows;
        let _ = render(1252.0, &mut app, &mut frame);
        let larger_rows = app.document.as_ref().unwrap().visible_rows;
        assert!(
            smaller_rows < reference_rows,
            "smaller window kept {smaller_rows} rows from the {reference_rows}-row reference"
        );
        assert!(
            larger_rows > reference_rows,
            "larger window kept {larger_rows} rows from the {reference_rows}-row reference"
        );

        app.document.as_mut().unwrap().shutdown();
        drop(app);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_controls_are_accessible_and_clickable() {
        let mut query = "needle".to_owned();
        let find = click_accessible_button("Find Next", |ui| {
            search_controls(ui, &mut query, true, None, None)
        });
        assert!(matches!(find, Some(Action::FindNext)));

        let progress = SearchProgress {
            bytes_scanned: 50,
            total_bytes: 100,
            rows_scanned: 4,
            elapsed: Duration::from_millis(1),
            done: false,
            cancelled: false,
        };
        let cancel = click_accessible_button("Cancel Search", |ui| {
            search_controls(ui, &mut query, true, Some(&progress), None)
        });
        assert!(matches!(cancel, Some(Action::CancelSearch)));
    }

    #[test]
    fn find_next_navigates_resumes_resets_and_reports_no_match() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-find-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "one,two,three,four").unwrap();
        for row in 1..=50 {
            match row {
                25 => writeln!(file, "row25,x,needle,x").unwrap(),
                30 => writeln!(file, "fresh,needle,x,x").unwrap(),
                _ => writeln!(file, "row{row},x,x,x").unwrap(),
            }
        }
        drop(file);

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.set_visible_rows(5).unwrap();

        document.start_find_next(b"needle").unwrap();
        assert!(!document.header_is_editable(0));
        assert_eq!(
            document.rename_header(0, "renamed".into()).unwrap_err(),
            "Wait for the search to finish before editing headers."
        );
        finish_search(&mut document);
        let first = document.last_match.as_ref().unwrap();
        assert_eq!((first.row, first.column), (25, 2));
        assert_eq!(document.viewport_start, 25);
        assert_eq!(document.reveal_cell, Some((25, 2)));
        assert_eq!(
            document.search_status.as_deref(),
            Some("Found row 25, column 3.")
        );

        document.start_find_next(b"needle").unwrap();
        finish_search(&mut document);
        let second = document.last_match.as_ref().unwrap();
        assert_eq!((second.row, second.column), (30, 1));
        assert_eq!(document.viewport_start, 30);

        document.start_find_next(b"fresh").unwrap();
        finish_search(&mut document);
        let reset = document.last_match.as_ref().unwrap();
        assert_eq!((reset.row, reset.column), (30, 0));

        document.set_visible_rows(25).unwrap();
        document.start_find_next(b"row50").unwrap();
        finish_search(&mut document);
        assert_eq!(document.viewport_start, 26);
        document.set_visible_rows(10).unwrap();
        assert_eq!(document.viewport_start, 41);
        assert_eq!(document.reveal_cell, Some((50, 0)));

        let before = document.viewport_start;
        document.start_find_next(b"missing").unwrap();
        finish_search(&mut document);
        assert_eq!(document.viewport_start, before);
        assert!(document.last_match.is_none());
        assert_eq!(
            document.search_status.as_deref(),
            Some("No further matches.")
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancel_search_action_sets_state_and_shutdown_clears_the_job() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "quarry-search-cancel-{}-{name}.csv",
            std::process::id()
        ));
        fs::write(&path, b"name\nvalue\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        let mut app = QuarryApp::new(None, Instant::now());
        app.find_input = "missing".into();
        app.document = Some(document);
        let ctx = egui::Context::default();
        app.apply(&ctx, Action::FindNext);
        app.apply(&ctx, Action::CancelSearch);
        assert!(
            app.document
                .as_ref()
                .unwrap()
                .search_progress()
                .unwrap()
                .cancelled
        );

        app.document.as_mut().unwrap().shutdown();
        assert!(app.document.as_ref().unwrap().search_job.is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_reveals_a_match_beyond_the_first_column_window() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-find-wide-{name}.csv"));
        let headers = (1..=40)
            .map(|column| format!("c{column}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut values = vec!["x"; 40];
        values[39] = "needle";
        fs::write(&path, format!("{headers}\n{}\n", values.join(","))).unwrap();

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.set_column_shown(39, false).unwrap();
        assert!(document.columns.hidden[39]);
        document.start_find_next(b"needle").unwrap();
        finish_search(&mut document);

        let found = document.last_match.as_ref().unwrap();
        assert_eq!((found.row, found.column), (1, 39));
        assert!(!document.columns.hidden[39]);
        assert!(document.columns.visible.contains(&39));
        assert_eq!(document.columns.start, 8);
        assert_eq!(document.headers.first().map(String::as_str), Some("c9"));
        assert_eq!(document.headers.last().map(String::as_str), Some("c40"));
        assert_eq!(document.reveal_cell, Some((1, 39)));

        let ctx = egui::Context::default();
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 780.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    show_grid(ui, &mut document).unwrap();
                });
            },
        );
        assert!(document.reveal_cell.is_none());

        document.show_first_columns();
        assert_eq!(document.columns.start, 0);
        assert_eq!(document.headers.first().map(String::as_str), Some("c1"));
        assert_eq!(document.headers.last().map(String::as_str), Some("c32"));
        assert_eq!(document.reveal_cell, Some((1, 0)));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn column_controls_preserve_source_identity_and_row_copy_order() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-columns-{name}.csv"));
        let headers = (1..=40)
            .map(|column| format!("c{column}"))
            .collect::<Vec<_>>();
        let values = (1..=40)
            .map(|column| format!("v{column}"))
            .collect::<Vec<_>>();
        fs::write(
            &path,
            format!("{}\n{}\n", headers.join(","), values.join(",")),
        )
        .unwrap();

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        document.view_column(39).unwrap();
        assert_eq!(document.columns.start, 8);
        assert_eq!(document.columns.visible.len(), super::MAX_VISIBLE_COLUMNS);
        assert_eq!(document.headers.first().map(String::as_str), Some("c9"));
        assert_eq!(document.headers.last().map(String::as_str), Some("c40"));

        document.selection = Some(GridSelection::Cell { row: 1, column: 39 });
        assert_eq!(document.copy_selection_text().unwrap(), "v40");
        document.move_column(39, 30).unwrap();
        assert_eq!(&document.columns.order[29..33], &[29, 39, 30, 31]);
        assert_eq!(document.headers[22], "c40");
        assert_eq!(document.headers[23], "c31");
        assert_eq!(document.copy_selection_text().unwrap(), "v40");

        document.move_column(39, 8).unwrap();
        let header_ctx = egui::Context::default();
        let output = header_ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let rendered_text = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(rendered_text.windows(2).any(|texts| texts == ["40", "c40"]));
        assert!(!rendered_text.windows(2).any(|texts| texts == ["9", "c40"]));

        document.selection = Some(GridSelection::Row { row: 1 });
        assert_eq!(document.copy_selection_text().unwrap(), values.join("\t"));

        document.selection = Some(GridSelection::Cell { row: 1, column: 39 });
        document.set_column_shown(39, false).unwrap();
        assert!(document.selection.is_none());
        assert!(!document.columns.visible.contains(&39));
        document.set_column_shown(39, true).unwrap();
        document.view_column(39).unwrap();
        assert!(document.columns.visible.contains(&39));

        for column in 0..40 {
            document.set_column_shown(column, false).unwrap();
        }
        assert!(document.headers.is_empty());
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(grid_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show_grid(ui, &mut document).unwrap();
            });
        });
        let tree = output
            .platform_output
            .accesskit_update
            .expect("accessibility tree should be present");
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button && node.label() == Some("Select row 1")
        }));
        assert!(!tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button
                && node
                    .label()
                    .is_some_and(|label| label.starts_with("Select row 1, column"))
        }));

        document.reset_columns();
        assert_eq!(document.columns.start, 0);
        assert_eq!(document.columns.order, (0..40).collect::<Vec<_>>());
        assert!(document.columns.hidden.iter().all(|hidden| !hidden));
        assert_eq!(document.headers.first().map(String::as_str), Some("c1"));
        assert_eq!(document.headers.last().map(String::as_str), Some("c32"));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn logical_scrollbar_maps_the_full_row_range() {
        let data_start = 1;
        let total_rows = 1_000;
        let visible_rows = 100;

        assert_eq!(
            row_for_scroll_fraction(0.0, data_start, total_rows, visible_rows),
            1
        );
        assert_eq!(
            row_for_scroll_fraction(0.5, data_start, total_rows, visible_rows),
            451
        );
        assert_eq!(
            row_for_scroll_fraction(1.0, data_start, total_rows, visible_rows),
            901
        );
        assert_eq!(
            scroll_fraction_for_row(1, data_start, total_rows, visible_rows),
            0.0
        );
        assert_eq!(
            scroll_fraction_for_row(901, data_start, total_rows, visible_rows),
            1.0
        );

        assert_eq!(max_viewport_start(10, 100), 0);
        assert_eq!(
            row_for_scroll_fraction(1.0, data_start, 10, 100),
            data_start
        );
        assert_eq!(
            logical_viewport_start(u64::MAX, data_start, 10, 100),
            data_start
        );
    }

    #[test]
    fn logical_scrollbar_is_monotonic_at_117_million_rows() {
        let data_start = 1;
        let total_rows = 117_168_829;
        let visible_rows = 24;
        let final_row = data_start + total_rows - visible_rows as u64;
        assert_eq!(
            row_for_scroll_fraction(1.0, data_start, total_rows, visible_rows),
            final_row
        );

        let mut previous = data_start;
        for step in 0..=10_000 {
            let row = row_for_scroll_fraction(
                step as f64 / 10_000.0,
                data_start,
                total_rows,
                visible_rows,
            );
            assert!(row >= previous);
            assert!(row <= final_row);
            previous = row;
        }
        assert_eq!(previous, final_row);
        assert_eq!(row_for_scroll_fraction(1.0, 1, u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn continuous_scrolling_refills_stays_bounded_and_reaches_the_last_record() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-scroll-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "name,value").unwrap();
        for row in 1..=250 {
            writeln!(file, "row{row},{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        let index = document.job.take().unwrap().wait().unwrap();

        document.progress.rows_scanned = document.session.first_rows.len() as u64;
        let bootstrap_data_rows = document
            .session
            .first_rows
            .len()
            .saturating_sub(document.data_start as usize);
        document.set_visible_rows(bootstrap_data_rows + 1).unwrap();
        assert_eq!(document.visible_rows().len(), bootstrap_data_rows);

        document.index = Some(index);
        document.set_visible_rows(25).unwrap();
        assert_eq!(document.visible_rows().len(), 25);

        let capacity = document.visible_rows + 2 * super::OVERSCAN_ROWS;
        assert!(document.buffered_rows.len() <= capacity);

        let first = document.data_start;
        let row_stride = super::ROW_HEIGHT;
        document
            .scroll_by_points(-(row_stride - 1.0), row_stride)
            .unwrap();
        assert_eq!(document.viewport_start, first);
        document.scroll_by_points(-1.0, row_stride).unwrap();
        assert_eq!(document.viewport_start, first + 1);
        document.scroll_by_points(row_stride, row_stride).unwrap();
        assert_eq!(document.viewport_start, first);

        document.page(1).unwrap();
        assert_eq!(document.viewport_start, first + 25);
        assert!(document.buffered_rows.len() <= capacity);
        document.page(-1).unwrap();
        assert_eq!(document.viewport_start, first);

        let target = row_for_scroll_fraction(
            1.0,
            document.data_start,
            document.available_data_rows(),
            document.visible_rows,
        );
        document.navigate(target).unwrap();

        assert_eq!(document.display_end(), 250);
        assert_eq!(document.visible_rows().last().unwrap().fields[0], b"row250");
        assert!(document.buffered_rows.len() <= capacity);
        document
            .scroll_by_points(-1_000.0 * row_stride, row_stride)
            .unwrap();
        assert_eq!(document.viewport_start, target);
        assert!(document.buffered_rows.len() <= capacity);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn later_wider_rows_expand_visible_columns() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-ragged-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        let narrow_rows = super::BOOTSTRAP_ROWS + 2 * super::OVERSCAN_ROWS + 1;
        for row in 1..=narrow_rows {
            writeln!(file, "{row},{}", row + 100).unwrap();
        }
        writeln!(file, "{},121,visible", narrow_rows + 1).unwrap();
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        assert!(!document.session.dialect.has_header);
        assert_eq!(document.total_columns, 2);
        document.move_column(1, 0).unwrap();
        document.set_column_shown(0, false).unwrap();

        let index = document.job.take().unwrap().wait().unwrap();
        document.index = Some(index);
        document.navigate(narrow_rows as u64).unwrap();

        assert_eq!(document.total_columns, 3);
        assert_eq!(document.columns.order, [1, 0, 2]);
        assert!(document.columns.hidden[0]);
        assert_eq!(document.columns.visible, [1, 2]);
        assert_eq!(document.headers, ["Column 2", "Column 3"]);
        assert_eq!(
            document.visible_rows().last().unwrap().fields[2],
            b"visible"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn late_index_error_is_reported_as_failed() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-malformed-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "name,value").unwrap();
        for _ in 0..300_000 {
            file.write_all(b"a,1\n").unwrap();
        }
        file.write_all(b"\"unterminated").unwrap();
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        document.last_poll = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !document.job.as_ref().unwrap().progress().done {
            assert!(Instant::now() < deadline, "malformed indexing timed out");
            std::thread::yield_now();
        }
        document.cancel();
        let error = document.poll().unwrap_err();

        assert!(error.contains("unterminated quoted field"));
        assert!(document.job.is_none());
        assert!(document.index.is_none());
        assert_eq!(document.index_status(), "Index failed");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn empty_file_has_no_active_row() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-empty-{name}.csv"));
        File::create(&path).unwrap();

        let mut document = Document::prepare(&path, OpenOptions::default()).unwrap();
        assert!(document.job.is_none());
        document.start_indexing().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !document.job.as_ref().unwrap().progress().done {
            assert!(Instant::now() < deadline, "empty file indexing timed out");
            std::thread::yield_now();
        }
        document.poll().unwrap();
        document.set_visible_rows(25).unwrap();

        assert!(document.job.is_none());
        assert!(document.index.is_some());
        assert_eq!(document.index_status(), "Index complete");
        assert_eq!(document.indexed_fraction(), 1.0);
        assert_eq!(document.available_data_rows(), 0);
        assert!(document.visible_rows().is_empty());
        assert_eq!(document.display_start(), 0);
        assert_eq!(document.display_end(), 0);
        document.index = None;
        assert_eq!(document.index_status(), "Index failed");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn replaces_a_document_while_its_indexer_is_active() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("quarry-active-{name}-first.csv"));
        let second = std::env::temp_dir().join(format!("quarry-active-{name}-second.csv"));
        fs::write(&first, b"a,b\n".repeat(250_000)).unwrap();
        fs::write(&second, b"name,value\nsecond,2\n").unwrap();

        let mut current = Document::prepare(&first, OpenOptions::default()).unwrap();
        let job = current
            .session
            .start_indexing(IndexConfig {
                chunk_bytes: 1,
                checkpoint_every: 8,
                memory_budget_bytes: 64 * 1024,
            })
            .unwrap();
        current.progress = job.progress();
        current.job = Some(job);

        let deadline = Instant::now() + Duration::from_secs(2);
        let progress = loop {
            let progress = current.job.as_ref().unwrap().progress();
            if progress.rows_scanned >= 100 || progress.done {
                break progress;
            }
            assert!(Instant::now() < deadline, "index did not make progress");
            std::thread::yield_now();
        };
        assert!(!progress.done, "test file indexed before replacement");

        let mut app = QuarryApp::new(None, Instant::now());
        app.document = Some(current);
        app.open_path(second.clone()).unwrap();

        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), second);
        assert_eq!(document.buffered_rows[0].fields[0], b"second");

        app.document.as_mut().unwrap().shutdown();
        drop(app);
        for path in [first, second] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn failed_and_cancelled_opens_preserve_the_document_and_drops_open_one_file() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = std::env::temp_dir().join(format!("quarry-open-{name}-first.csv"));
        let second = std::env::temp_dir().join(format!("quarry-open-{name}-second.csv"));
        let malformed = std::env::temp_dir().join(format!("quarry-open-{name}-bad.csv"));
        fs::write(&first, b"name,value\nfirst,1\n").unwrap();
        fs::write(&second, b"name,value\nsecond,2\n").unwrap();
        fs::write(&malformed, b"\"unterminated").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.path_input = format!("  {}\n", first.display());
        app.open_typed_path();
        assert!(app.notice.is_none());
        app.document.as_mut().unwrap().move_column(1, 0).unwrap();
        app.document
            .as_mut()
            .unwrap()
            .set_column_shown(0, false)
            .unwrap();
        app.open_picker_result(None);
        assert_eq!(app.document.as_ref().unwrap().session.path(), first);

        app.open_path_and_report(malformed.clone());
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), first);
        assert_eq!(document.columns.order, [1, 0]);
        assert!(document.columns.hidden[0]);
        assert!(app.notice.as_deref().unwrap().contains("unterminated"));

        app.handle_dropped_paths(vec![Some(second.clone()), Some(first.clone())]);
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), second);
        assert_eq!(document.columns.order, [0, 1]);
        assert!(document.columns.hidden.iter().all(|hidden| !hidden));
        assert!(app.notice.as_deref().unwrap().contains("ignored 1"));

        app.handle_dropped_paths(vec![None]);
        assert_eq!(app.document.as_ref().unwrap().session.path(), second);
        assert!(app.notice.as_deref().unwrap().contains("local file"));

        app.document.as_mut().unwrap().shutdown();
        drop(app);
        for path in [first, second, malformed] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn format_changes_wait_for_reopen() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-format-{name}.csv"));
        fs::write(&path, b"name,value\nfirst,1\n").unwrap();

        let mut app = QuarryApp::new(None, Instant::now());
        app.open_path(path.clone()).unwrap();
        assert_eq!(
            app.document.as_ref().unwrap().session.dialect.delimiter,
            b','
        );
        assert!(app.document.as_ref().unwrap().session.dialect.has_header);

        app.delimiter_mode = DelimiterMode::Tab;
        app.header_mode = HeaderMode::NoHeader;
        assert_eq!(
            app.document.as_ref().unwrap().session.dialect.delimiter,
            b','
        );
        assert!(app.document.as_ref().unwrap().session.dialect.has_header);

        app.reopen_document();
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.session.path(), path);
        assert_eq!(document.session.dialect.delimiter, b'\t');
        assert!(!document.session.dialect.has_header);
        assert_eq!(document.headers, ["Column 1"]);
        assert_eq!(document.buffered_rows[0].fields[0], b"name,value");

        app.delimiter_mode = DelimiterMode::Comma;
        app.header_mode = HeaderMode::FirstRow;
        app.reopen_document();
        let document = app.document.as_ref().unwrap();
        assert_eq!(document.headers, ["name", "value"]);
        assert_eq!(document.buffered_rows[0].fields[0], b"first");
        assert_eq!(document.buffered_rows[0].fields[1], b"1");
        assert_eq!(DelimiterMode::Pipe.delimiter(), Some(b'|'));
        assert_eq!(DelimiterMode::Semicolon.delimiter(), Some(b';'));

        app.document.as_mut().unwrap().shutdown();
        drop(app);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn row_and_column_inputs_are_one_based() {
        assert_eq!(parse_data_row("1", 1).unwrap(), 1);
        assert_eq!(parse_data_row("100000000", 1).unwrap(), 100_000_000);
        assert_eq!(parse_data_row("0", 1).unwrap_err(), "Data rows start at 1.");
        assert_eq!(parse_file_column("40", 40).unwrap(), 39);
        assert_eq!(
            parse_file_column("0", 40).unwrap_err(),
            "File columns start at 1."
        );
        assert_eq!(
            parse_file_column("41", 40).unwrap_err(),
            "File column must be between 1 and 40."
        );
        assert_eq!(parse_column_position("40", 40).unwrap(), 39);
        assert_eq!(
            parse_column_position("0", 40).unwrap_err(),
            "Display positions start at 1."
        );
        assert_eq!(
            parse_column_position("41", 40).unwrap_err(),
            "Display position must be between 1 and 40."
        );
    }
}
