//! Virtualized `DataTable` delegate backed directly by `WorkbenchController` and
//! `quarry_workbench::Document` (zero duplicate document or worker thread).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::{
    App, Context, Entity, FontFeatures, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, SharedString, Styled, Window, div, px, rgb,
};
use quarry_workbench::{
    Document, MAX_GRID_TEXT_CHARS, WindowId, field_preview, source_looks_numeric, whitespace,
};

use crate::workbench::WorkbenchController;

/// OpenType `tnum` (tabular figures) font feature configuration for native CoreText
/// and `cosmic-text` shaping so numeric columns and row numbers stay vertically aligned.
pub fn tabular_font_features() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".to_owned(), 1)]))
}

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

    pub fn visible_rows_count(&self) -> usize {
        self.visible_rows.len().max(40)
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

    /// Maps a physical `(source_row, source_column)` to rendered `(table_row, table_column)` when visible.
    pub fn display_position_for_source(
        &self,
        source_row: u64,
        source_col: usize,
        cx: &App,
    ) -> Option<(usize, usize)> {
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        let table_col = doc
            .columns
            .visible
            .iter()
            .position(|&c| c == source_col)
            .map(|ix| ix + 1)?;
        let table_row = if doc.filter_active() {
            doc.filtered_rows
                .iter()
                .position(|r| r.row == source_row)
                .map(|rel| (doc.filter_buffer_start as usize).saturating_add(rel))?
        } else {
            usize::try_from(source_row.checked_sub(doc.data_start)?).ok()?
        };
        Some((table_row, table_col))
    }

    /// Returns the source row index for rendered `table_row` if loaded.
    pub fn source_row_at(&self, row: usize, cx: &App) -> Option<u64> {
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        loaded_row(doc, row).map(|(source_row, _)| source_row)
    }

    fn display_loaded(&self, row: usize, column: usize, cx: &App) -> Option<String> {
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        let (source_row, fields) = loaded_row(doc, row)?;
        if column == 0 {
            if let Some(global_row) = doc.global_data_row_number(source_row) {
                return Some(global_row.to_string());
            }
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
            if let Some(global_row) = doc.global_data_row_number(source_row) {
                return Some(global_row.to_string());
            }
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
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id);
        if column == 0 {
            let gutter_width = doc
                .map(|doc| {
                    let last_source = doc
                        .data_start
                        .saturating_add(doc.available_data_rows().saturating_sub(1));
                    let max_ordinal = doc
                        .global_data_row_number(last_source)
                        .unwrap_or_else(|| doc.available_data_rows().max(1));
                    let digits = max_ordinal.to_string().len();
                    (digits as f32 * 9.5 + 28.0).clamp(58.0, 140.0)
                })
                .unwrap_or(58.0);
            return Column::new("source-row", "#")
                .width(px(gutter_width))
                .fixed_left()
                .resizable(false)
                .movable(false)
                .selectable(false)
                .text_right();
        }
        let Some(doc) = doc else {
            return Column::new("empty", "");
        };
        let Some(&source) = doc.columns.visible.get(column - 1) else {
            return Column::new(format!("missing-{column}"), "");
        };
        let selected = doc.selected_columns.contains(&source);
        let title = if selected {
            format!("✓ {}", doc.column_name(source))
        } else {
            doc.column_name(source)
        };
        Column::new(format!("source-{source}"), title)
            .width(self.widths.get(&source).copied().unwrap_or(px(124.)))
            .min_width(px(60.))
            .max_width(px(900.))
    }

    fn cell_label(&self, row: usize, column: usize, cx: &App) -> Option<SharedString> {
        let value = self.display_raw_loaded(row, column, cx)?;
        if column == 0 {
            if value.is_empty() {
                return None;
            }
            let bookmarked = self.source_row_at(row, cx).is_some_and(|src_row| {
                self.controller
                    .read(cx)
                    .active_document(self.window_id)
                    .is_some_and(|doc| doc.bookmarks.contains_row(src_row))
            });
            return Some(if bookmarked {
                format!("Row {value}, bookmarked").into()
            } else {
                format!("Row {value}").into()
            });
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

    fn render_th(
        &mut self,
        column: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let name = self.column(column, cx).name.clone();
        if column == 0 {
            return div()
                .id("grid-th-row-num")
                .size_full()
                .flex()
                .items_center()
                .justify_end()
                .child(name);
        }
        let source_col = self.source_column_at(column, cx);
        let selected = source_col.is_some_and(|src| {
            self.controller
                .read(cx)
                .active_document(self.window_id)
                .is_some_and(|doc| doc.selected_columns.contains(&src))
        });
        let mut header = div()
            .id(("grid-th", column))
            .size_full()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .cursor_pointer()
            .text_color(if selected {
                cx.theme().primary
            } else {
                cx.theme().foreground
            })
            .child(name);
        if let Some(src_col) = source_col {
            let controller = self.controller.clone();
            let win_id = self.window_id;
            header = header.on_mouse_down(MouseButton::Left, move |event, _, cx| {
                let shift = event.modifiers.shift;
                let toggle = event.modifiers.platform || event.modifiers.control;
                controller.update(cx, |controller, cx| {
                    controller.select_column_active(win_id, src_col, shift, toggle, cx);
                });
            });
        }
        header
    }

    fn render_td(
        &mut self,
        row: usize,
        column: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let text = self.display_loaded(row, column, cx).unwrap_or_default();
        let (has_whitespace_mark, numeric_cell) = if column > 0 {
            let show_all = self.controller.read(cx).show_all_whitespace();
            let raw = self.display_raw_loaded(row, column, cx).unwrap_or_default();
            let is_numeric = source_looks_numeric(raw.as_bytes());
            let has_ws = if raw.is_empty() {
                false
            } else {
                let summary =
                    whitespace::analyze(raw.as_bytes(), &std::sync::atomic::AtomicBool::new(false));
                summary.is_some_and(|s| s.indicated(show_all))
            };
            (has_ws, is_numeric)
        } else {
            (false, true)
        };
        let tokens = self.controller.read(cx).active_palette();
        let (row_selected, row_bookmarked) = if column == 0 {
            self.source_row_at(row, cx)
                .map_or((false, false), |src_row| {
                    self.controller
                        .read(cx)
                        .active_document(self.window_id)
                        .map_or((false, false), |doc| {
                            (
                                doc.selected_rows.contains(src_row),
                                doc.bookmarks.contains_row(src_row),
                            )
                        })
                })
        } else {
            (false, false)
        };
        let mut cell = div()
            .id(("grid-cell", row * 1024 + column))
            .relative()
            .size_full()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(quarry_workbench::GRID_FONT_SIZE))
            .font_features(tabular_font_features())
            .text_color(if column == 0 {
                if row_selected {
                    rgb(tokens.on_accent.to_rgb_hex()).into()
                } else {
                    rgb(tokens.accent_text.to_rgb_hex()).into()
                }
            } else {
                cx.theme().foreground
            });
        if row_selected {
            cell = cell.bg(rgb(tokens.selection_strong.to_rgb_hex()));
        }
        if column == 0
            && let Some(src_row) = self.source_row_at(row, cx)
        {
            let controller = self.controller.clone();
            let win_id = self.window_id;
            let right_controller = self.controller.clone();
            cell = cell
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                    let shift = event.modifiers.shift;
                    let toggle = event.modifiers.platform || event.modifiers.control;
                    controller.update(cx, |controller, cx| {
                        controller.select_row_active(win_id, src_row, shift, toggle, cx);
                    });
                })
                .on_mouse_down(MouseButton::Right, move |_, _, cx| {
                    right_controller.update(cx, |controller, cx| {
                        let _ = controller.toggle_bookmark_row_active(win_id, src_row, cx);
                    });
                });
        }
        if row_bookmarked {
            cell = cell.child(
                div()
                    .w(px(6.))
                    .h(px(6.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(if row_selected {
                        tokens.on_accent.to_rgb_hex()
                    } else {
                        tokens.accent_text.to_rgb_hex()
                    })),
            );
        }
        let mut text_div = div().flex_1().min_w_0().overflow_hidden().text_ellipsis();
        if numeric_cell {
            text_div = text_div.text_right();
        }
        cell = cell.child(text_div.child(text));
        if has_whitespace_mark {
            cell = cell.child(
                div()
                    .absolute()
                    .top(px(2.))
                    .right(px(2.))
                    .w(px(5.))
                    .h(px(5.))
                    .rounded_full()
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
