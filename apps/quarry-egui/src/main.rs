use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, FontFamily, FontId, Layout, RichText, TextStyle};
use egui_extras::{Column, TableBuilder};
use quarry_core::{
    IndexConfig, IndexJob, IndexProgress, OpenOptions, Row, Session, StructuralIndex,
};

const INITIAL_VISIBLE_ROWS: usize = 15;
const OVERSCAN_ROWS: usize = 2;
const ROW_HEIGHT: f32 = 25.0;
const HEADER_HEIGHT: f32 = 30.0;
const GRID_TITLE_HEIGHT: f32 = 36.0;
const SCROLLBAR_WIDTH: f32 = 18.0;
const MIN_THUMB_HEIGHT: f32 = 24.0;
const MAX_VISIBLE_COLUMNS: usize = 32;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

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
        "Quarry — egui prototype",
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
            document: None,
            notice: None,
            started,
            logged_first_update: false,
        };
        if initial_path.is_some() {
            app.open_document();
        }
        app
    }

    fn open_document(&mut self) {
        if self.path_input.trim().is_empty() {
            self.notice = Some("Enter a file path to open.".into());
            return;
        }
        match Document::open(Path::new(self.path_input.trim())) {
            Ok(document) => {
                self.jump_input = "1".into();
                self.document = Some(document);
                self.notice = None;
            }
            Err(error) => self.notice = Some(error),
        }
    }

    fn apply(&mut self, action: Action) {
        if action == Action::Open {
            self.open_document();
            return;
        }
        let Some(document) = self.document.as_mut() else {
            return;
        };
        let result = match action {
            Action::Open => unreachable!(),
            Action::PageUp => document.page(-1),
            Action::PageDown => document.page(1),
            Action::Jump => parse_data_row(&self.jump_input, document.data_start)
                .and_then(|start| document.navigate(start)),
            Action::Cancel => {
                document.cancel();
                Ok(())
            }
        };
        self.notice = result.err();
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

        if let Some(document) = &mut self.document {
            if let Err(error) = document.poll() {
                self.notice = Some(error);
            }
            if document.is_indexing() {
                ctx.request_repaint_after(POLL_INTERVAL);
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
                        RichText::new("EGUI BAKE-OFF")
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
                    let width = (ui.available_width() - 78.0).max(200.0);
                    let response = ui
                        .add_sized(
                            [width, 28.0],
                            egui::TextEdit::singleline(&mut self.path_input)
                                .hint_text("/path/to/file.csv"),
                        )
                        .labelled_by(label.id);
                    if ui.button("Open").clicked()
                        || (response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    {
                        action = Some(Action::Open);
                    }
                });

                if let Some(document) = &self.document {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let progress = document.indexed_fraction();
                        let status = if document.progress.cancelled {
                            "Index cancelled"
                        } else if document.is_indexing() {
                            "Indexing"
                        } else {
                            "Index complete"
                        };
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
                                    .horizontal_align(Align::RIGHT),
                            )
                            .labelled_by(label.id);
                        if ui.button("Jump").clicked()
                            || (jump.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                        {
                            action = Some(Action::Jump);
                        }
                        ui.label(
                            RichText::new("Page Up / Page Down")
                                .monospace()
                                .size(10.0)
                                .color(Color32::from_rgb(89, 103, 111)),
                        );
                    });
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
            self.apply(action);
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
        if grid_error.is_some() {
            self.notice = grid_error;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Open,
    PageUp,
    PageDown,
    Jump,
    Cancel,
}

struct Document {
    session: Session,
    job: Option<IndexJob>,
    index: Option<StructuralIndex>,
    progress: IndexProgress,
    headers: Vec<String>,
    total_columns: usize,
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
    fn open(path: &Path) -> Result<Self, String> {
        let buffer_rows = INITIAL_VISIBLE_ROWS + 2 * OVERSCAN_ROWS;
        let session = Session::open(
            path,
            OpenOptions {
                rows: buffer_rows + 1,
                ..OpenOptions::default()
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
        let headers = headers_for(&session, total_columns);
        let buffered_rows = session
            .first_rows
            .iter()
            .skip(data_start as usize)
            .take(buffer_rows)
            .cloned()
            .collect();
        let job = session
            .start_indexing(IndexConfig::default())
            .map_err(|error| error.to_string())?;
        let progress = job.progress();

        Ok(Self {
            session,
            job: Some(job),
            index: None,
            progress,
            headers,
            total_columns,
            data_start,
            viewport_start: data_start,
            buffer_start: data_start,
            buffered_rows,
            visible_rows: INITIAL_VISIBLE_ROWS,
            scroll_points: 0.0,
            last_viewport_read: None,
            last_poll: Instant::now(),
        })
    }

    fn poll(&mut self) -> Result<(), String> {
        if self.last_poll.elapsed() < POLL_INTERVAL {
            return Ok(());
        }
        self.last_poll = Instant::now();
        let Some(job) = &self.job else {
            return Ok(());
        };
        self.progress = job.progress();
        if self.progress.done {
            let job = self.job.take().expect("index job is present");
            self.index = Some(job.wait().map_err(|error| error.to_string())?);
        }
        Ok(())
    }

    fn is_indexing(&self) -> bool {
        self.job.is_some() && !self.progress.done
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
        self.progress.bytes_scanned as f32 / self.session.file_size.max(1) as f32
    }

    fn set_visible_rows(&mut self, visible_rows: usize) -> Result<(), String> {
        self.visible_rows = visible_rows.max(1);
        if self.available_data_rows() == 0 {
            return Ok(());
        }
        self.navigate(self.viewport_start)
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

fn headers_for(session: &Session, total_columns: usize) -> Vec<String> {
    // ponytail: render 32 columns in this spike; add horizontal column virtualization if egui wins.
    let visible = total_columns.min(MAX_VISIBLE_COLUMNS);
    if session.dialect.has_header {
        return session
            .first_rows
            .first()
            .map(|row| {
                row.fields
                    .iter()
                    .take(visible)
                    .enumerate()
                    .map(|(index, field)| {
                        let text = field_text(field);
                        if text.is_empty() {
                            format!("Column {}", index + 1)
                        } else {
                            text
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    (1..=visible)
        .map(|index| format!("Column {index}"))
        .collect()
}

fn show_grid(ui: &mut egui::Ui, document: &mut Document) -> Result<(), String> {
    let grid_height = ui.available_height();
    let horizontal_scrollbar = ui.spacing().scroll.allocated_width();
    let body_height =
        (grid_height - GRID_TITLE_HEIGHT - HEADER_HEIGHT - horizontal_scrollbar).max(ROW_HEIGHT);
    let row_stride = ROW_HEIGHT + ui.spacing().item_spacing.y;
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

        ui.separator();
        ui.allocate_ui_with_layout(ui.available_size(), Layout::top_down(Align::Min), |ui| {
            show_table(ui, document)
        });
        Ok(())
    })
    .inner
}

fn show_table(ui: &mut egui::Ui, document: &Document) {
    let rows = document.visible_rows();
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
        if document.total_columns > MAX_VISIBLE_COLUMNS {
            ui.label(format!(
                "Showing first {MAX_VISIBLE_COLUMNS} of {} columns",
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
                        table_row.col(|ui| {
                            ui.label(
                                RichText::new((start + row_index as u64).to_string())
                                    .monospace()
                                    .color(Color32::from_rgb(49, 85, 217)),
                            );
                        });
                        for column in 0..document.headers.len() {
                            table_row.col(|ui| {
                                let text = row
                                    .fields
                                    .get(column)
                                    .map_or_else(String::new, |field| field_text(field));
                                ui.label(RichText::new(text).monospace());
                            });
                        }
                    });
                });
        });
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
    visuals.selection.stroke = egui::Stroke::new(1.0, Color32::WHITE);
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
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(200, 209, 213)))
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        Document, logical_viewport_start, max_viewport_start, parse_data_row,
        row_for_scroll_fraction, scroll_fraction_for_row,
    };

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

        let mut document = Document::open(&path).unwrap();
        let index = document.job.take().unwrap().wait().unwrap();

        document.progress.rows_scanned = document.session.first_rows.len() as u64;
        document.set_visible_rows(25).unwrap();
        assert_eq!(document.visible_rows().len(), 19);

        document.index = Some(index);
        document.set_visible_rows(25).unwrap();
        assert_eq!(document.visible_rows().len(), 25);

        let capacity = document.visible_rows + 2 * super::OVERSCAN_ROWS;
        assert!(document.buffered_rows.len() <= capacity);

        let first = document.data_start;
        let row_stride = super::ROW_HEIGHT + 6.0;
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
    fn empty_file_has_no_active_row() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-empty-{name}.csv"));
        File::create(&path).unwrap();

        let mut document = Document::open(&path).unwrap();
        let index = document.job.take().unwrap().wait().unwrap();
        document.index = Some(index);
        document.set_visible_rows(25).unwrap();

        assert_eq!(document.available_data_rows(), 0);
        assert!(document.visible_rows().is_empty());
        assert_eq!(document.display_start(), 0);
        assert_eq!(document.display_end(), 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn jump_rows_are_one_based() {
        assert_eq!(parse_data_row("1", 1).unwrap(), 1);
        assert_eq!(parse_data_row("100000000", 1).unwrap(), 100_000_000);
        assert_eq!(parse_data_row("0", 1).unwrap_err(), "Data rows start at 1.");
    }
}
