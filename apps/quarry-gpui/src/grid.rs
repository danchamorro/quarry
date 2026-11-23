//! Virtualized `DataTable` delegate backed directly by `WorkbenchController` and
//! `quarry_workbench::Document` (zero duplicate document or worker thread).

use std::collections::HashMap;
use std::ops::Range;

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::{
    App, Context, Entity, IntoElement, ParentElement, Pixels, SharedString, Styled, Window, div,
    px, rgb,
};
use quarry_workbench::{Document, MAX_GRID_TEXT_CHARS, WindowId, field_preview, whitespace};

use crate::workbench::WorkbenchController;

pub struct Grid {
    controller: Entity<WorkbenchController>,
    window_id: WindowId,
    widths: HashMap<usize, Pixels>,
    visible_rows: Range<usize>,
    visible_columns: Range<usize>,
}

impl Grid {
    pub fn new(controller: Entity<WorkbenchController>, window_id: WindowId) -> Self {
        Self {
            controller,
            window_id,
            widths: HashMap::new(),
            visible_rows: 0..40,
            visible_columns: 0..16,
        }
    }

    pub fn columns(&self, cx: &App) -> Vec<usize> {
        self.controller
            .read(cx)
            .active_document(self.window_id)
            .map(|doc| doc.columns.visible.clone())
            .unwrap_or_default()
    }

    pub fn set_widths(&mut self, widths: &[Pixels], cx: &App) {
        let shown = self.columns(cx);
        for (source, width) in shown.iter().zip(widths.iter().skip(1)) {
            self.widths.insert(*source, *width);
        }
    }

    pub fn reset_widths(&mut self) {
        self.widths.clear();
    }

    pub fn auto_fit(&mut self, cx: &App) {
        let controller = self.controller.read(cx);
        let Some(doc) = controller.active_document(self.window_id) else {
            return;
        };
        let shown = &doc.columns.visible;
        for &source in shown {
            let mut chars = doc.column_name(source).chars().count();
            for row_ix in 0..doc.visible_row_count() {
                if let Some((source_row, fields)) = doc.visible_row(row_ix) {
                    let raw = fields.get(source).map(Vec::as_slice).unwrap_or(&[]);
                    let value = doc.cell_value(source_row, source, raw);
                    chars = chars.max(field_preview(value, 80).chars().count());
                }
            }
            self.widths.insert(
                source,
                px((chars.min(48) as f32 * 8.2 + 24.).clamp(76., 420.)),
            );
        }
    }

    /// Maps a rendered `(row, table_column)` to `(source_row, source_column)` if loaded.
    pub fn cell_at(&self, row: usize, column: usize, cx: &App) -> Option<(u64, usize)> {
        if column == 0 {
            return None;
        }
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        let source_col = *doc.columns.visible.get(column - 1)?;
        let (source_row, _) = loaded_row(doc, row)?;
        Some((source_row, source_col))
    }

    /// Maps a rendered `table_column` (1-based) to its physical `source_column`.
    pub fn source_column_at(&self, column: usize, cx: &App) -> Option<usize> {
        if column == 0 {
            return None;
        }
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        doc.columns.visible.get(column - 1).copied()
    }

    fn display_loaded(&self, row: usize, column: usize, cx: &App) -> Option<String> {
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        let (source_row, fields) = loaded_row(doc, row)?;
        if column == 0 {
            return Some((source_row + 1 - doc.data_start).to_string());
        }
        let source_col = *doc.columns.visible.get(column - 1)?;
        let raw = fields.get(source_col).map(Vec::as_slice).unwrap_or(&[]);
        let value = doc.cell_value(source_row, source_col, raw);
        let preview = field_preview(value, MAX_GRID_TEXT_CHARS.min(240));
        if controller.show_all_whitespace() {
            Some(render_whitespace_marks(&preview))
        } else {
            Some(preview)
        }
    }

    fn display_raw_loaded(&self, row: usize, column: usize, cx: &App) -> Option<String> {
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        let (source_row, fields) = loaded_row(doc, row)?;
        if column == 0 {
            return Some((source_row + 1 - doc.data_start).to_string());
        }
        let source_col = *doc.columns.visible.get(column - 1)?;
        let raw = fields.get(source_col).map(Vec::as_slice).unwrap_or(&[]);
        let value = doc.cell_value(source_row, source_col, raw);
        Some(field_preview(value, MAX_GRID_TEXT_CHARS.min(240)))
    }
}

fn loaded_row(doc: &Document, table_row: usize) -> Option<(u64, &[Vec<u8>])> {
    if doc.filter_active() {
        let rel = (table_row as u64).checked_sub(doc.filter_buffer_start)?;
        let index = usize::try_from(rel).ok()?;
        let found = doc.filtered_rows.get(index)?;
        Some((found.row, found.fields.as_slice()))
    } else {
        let source_row = doc.data_start.checked_add(table_row as u64)?;
        let rel = source_row.checked_sub(doc.buffer_start)?;
        let index = usize::try_from(rel).ok()?;
        let found = doc.buffered_rows.get(index)?;
        Some((source_row, found.fields.as_slice()))
    }
}

fn render_whitespace_marks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            ' ' => out.push('·'),
            '\t' => out.push('→'),
            '\r' => out.push('↵'),
            '\n' => out.push('⏎'),
            '\u{a0}' => out.push('⍽'),
            other => out.push(other),
        }
    }
    out
}

impl TableDelegate for Grid {
    fn columns_count(&self, cx: &App) -> usize {
        self.controller
            .read(cx)
            .active_document(self.window_id)
            .map_or(1, |doc| doc.columns.visible.len() + 1)
    }

    fn rows_count(&self, cx: &App) -> usize {
        self.controller
            .read(cx)
            .active_document(self.window_id)
            .map_or(0, |doc| {
                usize::try_from(doc.grid_total_rows()).unwrap_or(usize::MAX)
            })
    }

    fn column(&self, column: usize, cx: &App) -> Column {
        if column == 0 {
            return Column::new("source-row", "#")
                .width(px(58.))
                .fixed_left()
                .resizable(false)
                .movable(false)
                .selectable(false)
                .text_right();
        }
        let controller = self.controller.read(cx);
        let Some(doc) = controller.active_document(self.window_id) else {
            return Column::new("empty", "");
        };
        let Some(&source) = doc.columns.visible.get(column - 1) else {
            return Column::new(format!("missing-{column}"), "");
        };
        let title = doc.column_name(source);
        Column::new(format!("source-{source}"), title)
            .width(self.widths.get(&source).copied().unwrap_or(px(124.)))
            .min_width(px(60.))
            .max_width(px(900.))
    }

    fn cell_label(&self, row: usize, column: usize, cx: &App) -> Option<SharedString> {
        let value = self.display_raw_loaded(row, column, cx)?;
        if column == 0 {
            return (!value.is_empty()).then(|| format!("Row {value}").into());
        }
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        let source = *doc.columns.visible.get(column - 1)?;
        let header = doc.column_name(source);
        if value.is_empty() {
            Some(format!("{header}: empty").into())
        } else {
            Some(format!("{header}: {value}").into())
        }
    }

    fn render_td(
        &mut self,
        row: usize,
        column: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let text = self.display_loaded(row, column, cx).unwrap_or_default();
        let has_whitespace_mark = if column > 0 {
            let show_all = self.controller.read(cx).show_all_whitespace();
            let raw = self.display_raw_loaded(row, column, cx).unwrap_or_default();
            if raw.is_empty() {
                false
            } else {
                let summary =
                    whitespace::analyze(raw.as_bytes(), &std::sync::atomic::AtomicBool::new(false));
                summary.is_some_and(|s| s.indicated(show_all))
            }
        } else {
            false
        };
        let tokens = &quarry_workbench::theme::LIGHT;
        let mut cell = div()
            .size_full()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(12.))
            .font_family("Menlo")
            .text_color(if column == 0 {
                rgb(0x9b7624).into()
            } else {
                cx.theme().foreground
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(text),
            );
        if has_whitespace_mark {
            cell = cell.child(
                div()
                    .w(px(5.))
                    .h(px(5.))
                    .rounded_full()
                    .flex_shrink_0()
                    .bg(rgb(tokens.indicator_fill.to_rgb_hex())),
            );
        }
        cell
    }

    fn visible_rows_changed(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.visible_rows = range.clone();
        let window_id = self.window_id;
        self.controller.update(cx, |controller, cx| {
            controller.request_viewport_rows(window_id, range.start as u64, range.len().max(1), cx);
        });
    }

    fn visible_columns_changed(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        self.visible_columns = range;
    }
}
