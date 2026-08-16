use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, FontFamily, FontId, Layout, RichText, TextStyle};
use egui_extras::{Column, TableBuilder};
use quarry_core::{
    HeaderMode, IndexConfig, IndexJob, IndexProgress, OpenOptions, Row, SearchJob, SearchMatch,
    SearchOutcome, SearchPosition, SearchProgress, Session, StructuralIndex,
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
    delimiter_mode: DelimiterMode,
    header_mode: HeaderMode,
    document: Option<Document>,
    notice: Option<String>,
    started: Instant,
    logged_first_update: bool,
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
            delimiter_mode: DelimiterMode::Auto,
            header_mode: HeaderMode::Auto,
            document: None,
            notice: None,
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
        let mut document = Document::prepare(&path, self.open_options())?;
        document.start_indexing()?;
        if let Some(current) = self.document.as_mut() {
            current.shutdown();
        }
        self.path_input = path.to_string_lossy().into_owned();
        self.jump_input = "1".into();
        self.column_input = "1".into();
        self.column_position_input = "1".into();
        self.columns_open = false;
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
        let path = rfd::FileDialog::new()
            .set_title("Open a delimited file")
            .pick_file();
        self.open_picker_result(path);
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
        match action {
            Action::Open => return self.open_typed_path(),
            Action::Choose => return self.choose_file(),
            Action::Reopen => return self.reopen_document(),
            Action::CopySelection => return self.copy_selection(ctx),
            Action::OpenColumns => {
                self.columns_open = true;
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
            | Action::CopySelection
            | Action::OpenColumns => {
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
                        ui.label(RichText::new("READ ONLY").monospace().size(10.0));
                    });
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
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
                    if ui.button("Choose…").clicked() {
                        action = Some(Action::Choose);
                    }
                    if ui.button("Open").clicked()
                        || (response.lost_focus()
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
                        .add_enabled(self.document.is_some(), egui::Button::new("Apply / Reopen"))
                        .clicked()
                    {
                        action = Some(Action::Reopen);
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
                        let label = ui.label("Data row");
                        let jump = ui
                            .add_sized(
                                [120.0, 26.0],
                                egui::TextEdit::singleline(&mut self.jump_input)
                                    .id(egui::Id::new(JUMP_INPUT_ID))
                                    .horizontal_align(Align::RIGHT),
                            )
                            .labelled_by(label.id);
                        if ui.button("Jump").clicked()
                            || (jump.lost_focus()
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
                        if let Some(copy_action) = copy_control(ui, document.selection.is_some()) {
                            action = Some(copy_action);
                        }
                    });
                    ui.add_space(6.0);
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
        let copy_event_targets_selection = self
            .document
            .as_ref()
            .and_then(|document| document.selection.as_ref())
            .is_some_and(|_| selection_copy_requested(ctx));
        if copy_event_targets_selection {
            self.copy_selection(ctx);
        }
        if grid_error.is_some() {
            self.notice = grid_error;
        }
        if self
            .document
            .as_ref()
            .is_some_and(|document| document.job.is_some() || document.search_job.is_some())
        {
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
    Jump,
    FindNext,
    CancelSearch,
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

fn selection_copy_requested(ctx: &egui::Context) -> bool {
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
        })
    });
    copy_event && !text_input_focused
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
                                let name = column_name(&document.session, column);
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
    reveal_cell: Option<(u64, usize)>,
    selection: Option<GridSelection>,
    headers: Vec<String>,
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
            reveal_cell: None,
            selection: None,
            headers,
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
        if self.search_job.is_some() {
            return Err("A search is already running.".into());
        }
        if !self.is_search_ready() {
            return Err("Search is available after indexing completes.".into());
        }
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

    fn is_search_ready(&self) -> bool {
        self.index.is_some() && self.progress.done && !self.progress.cancelled
    }

    fn cancel_search(&self) {
        if let Some(job) = &self.search_job {
            job.cancel();
        }
    }

    fn center_column(&mut self, column: usize) {
        self.ensure_column_count(column.saturating_add(1));
        self.columns.view(column);
        self.refresh_column_headers();
        self.clear_hidden_selection();
    }

    fn view_column(&mut self, column: usize) -> Result<(), String> {
        self.validate_column(column)?;
        self.columns.view(column);
        self.refresh_column_headers();
        self.reveal_cell = Some((self.viewport_start, column));
        self.clear_hidden_selection();
        Ok(())
    }

    fn set_column_shown(&mut self, column: usize, shown: bool) -> Result<(), String> {
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
        self.columns.first();
        self.refresh_column_headers();
        self.reveal_cell = self
            .columns
            .visible
            .first()
            .copied()
            .map(|column| (self.viewport_start, column));
        self.clear_hidden_selection();
    }

    fn reset_columns(&mut self) {
        self.columns.reset();
        self.refresh_column_headers();
        self.reveal_cell = self
            .columns
            .visible
            .first()
            .copied()
            .map(|column| (self.viewport_start, column));
        self.clear_hidden_selection();
    }

    fn ensure_column_count(&mut self, total_columns: usize) {
        if total_columns > self.total_columns {
            self.total_columns = total_columns;
            self.columns.extend_to(total_columns);
        }
    }

    fn refresh_column_headers(&mut self) {
        self.headers = headers_for(&self.session, &self.columns.visible);
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
        if let Some(job) = self.search_job.take() {
            job.cancel();
            drop(job);
        }
        if let Some(job) = self.job.take() {
            drop(job);
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

    fn visible_rows(&self) -> &[Row] {
        let offset = usize::try_from(self.viewport_start.saturating_sub(self.buffer_start))
            .unwrap_or(self.buffered_rows.len())
            .min(self.buffered_rows.len());
        let end = offset
            .saturating_add(self.visible_rows)
            .min(self.buffered_rows.len());
        &self.buffered_rows[offset..end]
    }

    fn clear_hidden_selection(&mut self) {
        let Some(selection) = self.selection else {
            return;
        };
        let row_end = self
            .viewport_start
            .saturating_add(self.visible_rows().len() as u64);
        let row_visible = (self.viewport_start..row_end).contains(&selection.row());
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
        let relative = selection
            .row()
            .checked_sub(self.buffer_start)
            .ok_or_else(|| "The selected row is no longer visible. Select it again.".to_owned())?;
        let offset = usize::try_from(relative)
            .map_err(|_| "The selected row is no longer visible. Select it again.".to_owned())?;
        let row = self
            .buffered_rows
            .get(offset)
            .ok_or_else(|| "The selected row is no longer visible. Select it again.".to_owned())?;
        selection_text(row, selection, MAX_COPY_BYTES)
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
    let total_rows = document.available_data_rows();

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
        let fraction = scroll_fraction_for_row(
            document.viewport_start,
            document.data_start,
            total_rows,
            document.visible_rows,
        );
        let mut slider_position = 1.0 - fraction;
        let thumb_height = scrollbar_thumb_height(grid_height, total_rows, document.visible_rows);
        let handle_radius = SCROLLBAR_WIDTH / 2.5;
        let scroll_enabled = total_rows > 0;
        let label = if scroll_enabled {
            format!(
                "Vertical scroll, row {} of {total_rows}",
                document.display_start()
            )
        } else {
            "Vertical scroll, no data rows".into()
        };
        let hover_text = if scroll_enabled {
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
                document.data_start,
                total_rows,
                document.visible_rows,
            );
            if target != document.viewport_start {
                document.navigate(target)?;
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
    document: &Document,
    reveal_cell: Option<(u64, usize)>,
) -> Option<GridSelection> {
    let rows = document.visible_rows();
    let mut clicked_selection = None;
    ui.horizontal(|ui| {
        if rows.is_empty() {
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
    let start = document.display_start();
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
                        ui.label(RichText::new("ROW").monospace().strong());
                    });
                    for name in &document.headers {
                        header.col(|ui| {
                            ui.label(RichText::new(name).strong());
                        });
                    }
                })
                .body(|body| {
                    body.rows(ROW_HEIGHT, rows.len(), |mut table_row| {
                        let row_index = table_row.index();
                        let row = &rows[row_index];
                        let record_row =
                            document.viewport_start.saturating_add(row_index as u64);
                        let display_row = start.saturating_add(row_index as u64);
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
                                    let text = row
                                        .fields
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
    match selection {
        GridSelection::Cell { column, .. } => {
            let mut output = String::new();
            append_clipboard_field(
                &mut output,
                row.fields.get(column).map_or(&[], Vec::as_slice),
                false,
                max_bytes,
            )?;
            Ok(output)
        }
        GridSelection::Row { .. } => {
            let mut output = String::new();
            for (index, field) in row.fields.iter().enumerate() {
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
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use eframe::egui;

    use super::{
        Action, COLUMN_INPUT_ID, COLUMN_POSITION_INPUT_ID, ColumnCommand, ColumnView,
        DelimiterMode, Document, FIND_INPUT_ID, GridSelection, HeaderMode, IndexConfig,
        OpenOptions, QuarryApp, Row, SearchProgress, column_drop_position, column_window_controls,
        copy_control, logical_viewport_start, max_viewport_start, page_controls,
        parse_column_position, parse_data_row, parse_file_column, row_for_scroll_fraction,
        scroll_fraction_for_row, search_controls, selection_text, show_column_manager, show_grid,
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
    fn maximized_reference_window_shows_at_least_40_rows() {
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
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1728.0, 1052.0),
                )),
                ..Default::default()
            },
            |ctx| eframe::App::update(&mut app, ctx, &mut frame),
        );

        let document = app.document.as_ref().unwrap();
        assert!(
            document.visible_rows >= 40,
            "maximized reference window fits only {} rows",
            document.visible_rows
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
