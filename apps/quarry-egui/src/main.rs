use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, FontFamily, FontId, Layout, RichText, TextStyle};
use egui_extras::{Column, TableBuilder};
use quarry_core::{
    IndexConfig, IndexJob, IndexProgress, OpenOptions, Row, Session, StructuralIndex,
};

const VIEWPORT_ROWS: usize = 100;
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
            Action::Previous => document.navigate(
                document
                    .viewport_start
                    .saturating_sub(VIEWPORT_ROWS as u64)
                    .max(document.data_start),
            ),
            Action::Next => {
                document.navigate(document.viewport_start.saturating_add(VIEWPORT_ROWS as u64))
            }
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
                        if ui.button("Previous").clicked() {
                            action = Some(Action::Previous);
                        }
                        if ui.button("Next").clicked() {
                            action = Some(Action::Next);
                        }
                        ui.separator();
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
                    Some(Action::Next)
                } else if input.key_pressed(egui::Key::PageUp) {
                    Some(Action::Previous)
                } else {
                    None
                }
            });
        }
        if let Some(action) = action {
            self.apply(action);
        }

        egui::CentralPanel::default()
            .frame(panel_frame(Color32::from_rgb(244, 247, 248)))
            .show(ctx, |ui| {
                if let Some(document) = &self.document {
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        Layout::left_to_right(Align::Min),
                        |ui| {
                            core_sample_rail(ui, document);
                            ui.separator();
                            ui.allocate_ui_with_layout(
                                ui.available_size(),
                                Layout::top_down(Align::Min),
                                |ui| show_grid(ui, document),
                            );
                        },
                    );
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            ui.heading("Open a delimited file");
                            ui.label("Quarry reads the first viewport before indexing the rest.");
                        });
                    });
                }
            });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Open,
    Previous,
    Next,
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
    rows: Vec<Row>,
    last_viewport_read: Option<Duration>,
    last_poll: Instant,
}

impl Document {
    fn open(path: &Path) -> Result<Self, String> {
        let session = Session::open(
            path,
            OpenOptions {
                rows: VIEWPORT_ROWS + 1,
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
        let rows = session
            .first_rows
            .iter()
            .skip(data_start as usize)
            .take(VIEWPORT_ROWS)
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
            rows,
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
    }

    fn available_data_rows(&self) -> u64 {
        self.available_rows().saturating_sub(self.data_start)
    }

    fn indexed_fraction(&self) -> f32 {
        self.progress.bytes_scanned as f32 / self.session.file_size.max(1) as f32
    }

    fn viewport_fraction(&self) -> f32 {
        self.rows
            .first()
            .map(|row| row.offset as f32 / self.session.file_size.max(1) as f32)
            .unwrap_or(0.0)
    }

    fn navigate(&mut self, requested: u64) -> Result<(), String> {
        let available = self.available_rows();
        let Some((start, count)) = viewport_request(requested, available, self.data_start) else {
            return Err(format!(
                "Data row {} is not indexed yet ({} available).",
                requested.saturating_sub(self.data_start).saturating_add(1),
                self.available_data_rows()
            ));
        };
        let began = Instant::now();
        let rows = if let Some(index) = &self.index {
            self.session.read_rows(index, start, count)
        } else if let Some(job) = &self.job {
            let index = job.snapshot();
            self.session.read_rows(&index, start, count)
        } else {
            return Err("No structural index is available.".into());
        }
        .map_err(|error| error.to_string())?;
        self.last_viewport_read = Some(began.elapsed());
        self.viewport_start = start;
        self.rows = rows;
        Ok(())
    }
}

fn viewport_request(requested: u64, available: u64, data_start: u64) -> Option<(u64, usize)> {
    let start = requested.max(data_start);
    if start >= available {
        return None;
    }
    let count = (available - start).min(VIEWPORT_ROWS as u64) as usize;
    Some((start, count))
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

fn show_grid(ui: &mut egui::Ui, document: &Document) {
    let start = document
        .viewport_start
        .saturating_sub(document.data_start)
        .saturating_add(1);
    let end = start.saturating_add(document.rows.len().saturating_sub(1) as u64);
    ui.horizontal(|ui| {
        ui.heading(format!("Rows {start}–{end}"));
        if document.total_columns > MAX_VISIBLE_COLUMNS {
            ui.label(format!(
                "Showing first {MAX_VISIBLE_COLUMNS} of {} columns",
                document.total_columns
            ));
        }
    });
    ui.add_space(6.0);

    let grid_height = ui.available_height();
    let column_width =
        ((ui.available_width() - 82.0) / document.headers.len().max(1) as f32).clamp(80.0, 160.0);
    let mut table = TableBuilder::new(ui)
        .id_salt("quarry-grid")
        .striped(true)
        .resizable(true)
        .auto_shrink([false, false])
        .min_scrolled_height(grid_height)
        .max_scroll_height(grid_height)
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
        .header(30.0, |mut header| {
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
            body.rows(25.0, document.rows.len(), |mut table_row| {
                let row_index = table_row.index();
                let row = &document.rows[row_index];
                table_row.col(|ui| {
                    ui.label(
                        RichText::new((start + row_index as u64).to_string())
                            .monospace()
                            .color(Color32::from_rgb(49, 85, 217)),
                    );
                });
                for field in row.fields.iter().take(document.headers.len()) {
                    table_row.col(|ui| {
                        ui.label(RichText::new(field_text(field)).monospace());
                    });
                }
            });
        });
}

fn core_sample_rail(ui: &mut egui::Ui, document: &Document) {
    ui.vertical(|ui| {
        ui.label(RichText::new("FILE").monospace().size(9.0));
        let height = ui.available_height().max(120.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(18.0, height), egui::Sense::hover());
        let inner = rect.shrink(3.0);
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(5),
            Color32::from_rgb(210, 219, 223),
        );
        let indexed_height = inner.height() * document.indexed_fraction().clamp(0.0, 1.0);
        if indexed_height > 0.0 {
            ui.painter().rect_filled(
                egui::Rect::from_min_max(
                    inner.min,
                    egui::pos2(inner.right(), inner.top() + indexed_height),
                ),
                egui::CornerRadius::same(2),
                Color32::from_rgb(79, 127, 131),
            );
        }
        let marker_y = inner.top() + inner.height() * document.viewport_fraction().clamp(0.0, 1.0);
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() - 2.0, marker_y),
                egui::pos2(rect.right() + 2.0, marker_y),
            ],
            egui::Stroke::new(3.0, Color32::from_rgb(49, 85, 217)),
        );
        response.on_hover_text(format!(
            "Indexed {:.1}% · viewport at {:.1}% of file bytes",
            document.indexed_fraction() * 100.0,
            document.viewport_fraction() * 100.0
        ));
    });
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
    use super::{VIEWPORT_ROWS, parse_data_row, viewport_request};

    #[test]
    fn viewport_requests_are_bounded_and_use_one_based_data_rows() {
        assert_eq!(viewport_request(0, 1_001, 1), Some((1, VIEWPORT_ROWS)));
        assert_eq!(viewport_request(950, 1_001, 1), Some((950, 51)));
        assert_eq!(viewport_request(1_001, 1_001, 1), None);
        assert_eq!(parse_data_row("1", 1).unwrap(), 1);
        assert_eq!(parse_data_row("100000000", 1).unwrap(), 100_000_000);
        assert_eq!(parse_data_row("0", 1).unwrap_err(), "Data rows start at 1.");
    }
}
