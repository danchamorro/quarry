//! Virtualized `DataTable` delegate backed directly by `WorkbenchController` and
//! `quarry_workbench::Document` (zero duplicate document or worker thread).

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::ActiveTheme;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    App, Context, Entity, FontFeatures, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, Point, SharedString, StatefulInteractiveElement, Styled, Window, div,
    px, rgb,
};
use quarry_workbench::{
    Document, MAX_GRID_TEXT_CHARS, WindowId, field_preview, source_looks_numeric,
};

use crate::whitespace_ui::render_whitespace;
use crate::workbench::WorkbenchController;

/// OpenType `tnum` (tabular figures) font feature configuration for native CoreText
/// and `cosmic-text` shaping so numeric columns and row numbers stay vertically aligned.
pub fn tabular_font_features() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".to_owned(), 1)]))
}

/// Interactive requests emitted from grid headers, row gutter cells, and data cells.
#[derive(Clone, Debug)]
pub enum GridAction {
    ColumnContextMenu {
        source_col: usize,
        position: Point<Pixels>,
    },
    RowContextMenu {
        table_row: usize,
        source_row: u64,
        position: Point<Pixels>,
    },
    CellContextMenu {
        table_row: usize,
        table_col: usize,
        source_row: u64,
        source_col: usize,
        position: Point<Pixels>,
    },
    RenameHeader {
        source_col: usize,
    },
}

/// Callback invoked when the user triggers a grid context menu or header rename.
pub type GridActionCallback = Rc<dyn Fn(GridAction, &mut Window, &mut App)>;

pub struct Grid {
    controller: Entity<WorkbenchController>,
    window_id: WindowId,
    on_action: GridActionCallback,
    gutter_mouse_row: Rc<Cell<Option<usize>>>,
    widths: HashMap<usize, Pixels>,
    visible_rows: Range<usize>,
    visible_columns: Range<usize>,
}

impl Grid {
    pub fn new(
        controller: Entity<WorkbenchController>,
        window_id: WindowId,
        on_action: GridActionCallback,
    ) -> Self {
        Self {
            controller,
            window_id,
            on_action,
            gutter_mouse_row: Rc::new(Cell::new(None)),
            widths: HashMap::new(),
            visible_rows: 0..40,
            visible_columns: 0..16,
        }
    }

    /// Records a mouse-driven gutter selection on `row` so the subsequent `TableEvent::SelectCell`
    /// does not overwrite modifier/toggle/multi-row selection.
    pub fn record_gutter_mouse_selection(&self, row: usize) {
        self.gutter_mouse_row.set(Some(row));
    }

    /// Consumes a recent mouse-click gutter selection on `row` so the bubbled `TableEvent::SelectCell`
    /// does not overwrite modifier/toggle mouse row selection.
    pub fn take_gutter_mouse_selection(&self, row: usize) -> bool {
        self.gutter_mouse_row.take() == Some(row)
    }

    /// Clears any recorded mouse-click gutter row marker when selection leaves column 0.
    pub fn clear_gutter_mouse_selection(&self) {
        self.gutter_mouse_row.set(None);
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

    pub fn visible_rows(&self) -> Range<usize> {
        self.visible_rows.clone()
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

    /// Maps a rendered `(row, table_column)` (for `column > 0`) to
    /// `(source_row, source_column, Option<raw_bytes>)`, where `None` indicates an absent/ragged field.
    pub fn cell_raw_value(
        &self,
        row: usize,
        column: usize,
        cx: &App,
    ) -> Option<(u64, usize, Option<Vec<u8>>)> {
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        let (source_row, source_col, raw) = borrowed_cell_raw(doc, row, column)?;
        Some((source_row, source_col, raw.map(<[u8]>::to_vec)))
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

    /// Maps a physical `source_row` to its rendered `table_row` index (including under filters).
    pub fn display_row_for_source(&self, source_row: u64, cx: &App) -> Option<usize> {
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        if doc.filter_active() {
            doc.filtered_rows
                .iter()
                .position(|r| r.row == source_row)
                .map(|rel| (doc.filter_buffer_start as usize).saturating_add(rel))
        } else {
            usize::try_from(source_row.checked_sub(doc.data_start)?).ok()
        }
    }

    /// Maps a physical `(source_row, source_column)` to rendered `(table_row, table_column)` when visible.
    pub fn display_position_for_source(
        &self,
        source_row: u64,
        source_col: usize,
        cx: &App,
    ) -> Option<(usize, usize)> {
        let table_row = self.display_row_for_source(source_row, cx)?;
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        let table_col = doc
            .columns
            .visible
            .iter()
            .position(|&c| c == source_col)
            .map(|ix| ix + 1)?;
        Some((table_row, table_col))
    }

    /// Returns the source row index for rendered `table_row` if loaded.
    pub fn source_row_at(&self, row: usize, cx: &App) -> Option<u64> {
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        loaded_row(doc, row).map(|(source_row, _)| source_row)
    }

    fn gutter_label(&self, row: usize, cx: &App) -> Option<String> {
        let controller = self.controller.read(cx);
        let doc = controller.active_document(self.window_id)?;
        let (source_row, _) = loaded_row(doc, row)?;
        if let Some(global_row) = doc.global_data_row_number(source_row) {
            return Some(global_row.to_string());
        }
        Some((source_row + 1 - doc.data_start).to_string())
    }
}

fn borrowed_cell_raw(
    doc: &Document,
    row: usize,
    column: usize,
) -> Option<(u64, usize, Option<&[u8]>)> {
    if column == 0 {
        return None;
    }
    let source_col = *doc.columns.visible.get(column - 1)?;
    let (source_row, fields) = loaded_row(doc, row)?;
    let raw = if let Some(edited) = doc.cell_edits.get(&(source_row, source_col)) {
        Some(edited.as_slice())
    } else {
        fields.get(source_col).map(Vec::as_slice)
    };
    Some((source_row, source_col, raw))
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
        if column == 0 {
            let value = self.gutter_label(row, cx)?;
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
        let (_, source_col, raw_opt) = borrowed_cell_raw(doc, row, column)?;
        let header = doc.column_name(source_col);
        let Some(raw) = raw_opt else {
            return Some(format!("{header}: Field is absent from this row.").into());
        };
        let preview = field_preview(raw, MAX_GRID_TEXT_CHARS.min(240));
        if preview.is_empty() {
            Some(format!("{header}: empty").into())
        } else {
            Some(format!("{header}: {preview}").into())
        }
    }

    fn render_th(
        &mut self,
        column: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if column == 0 {
            let name = self.column(column, cx).name.clone();
            return div()
                .id("grid-th-row-num")
                .size_full()
                .flex()
                .items_center()
                .justify_end()
                .child(name);
        }
        let source_col = self.source_column_at(column, cx);
        let (selected, header_name, header_len, tokens, show_all) = {
            let controller = self.controller.read(cx);
            let tokens = controller.active_palette();
            let show_all = controller.show_all_whitespace();
            let doc = controller.active_document(self.window_id);
            let selected = source_col
                .is_some_and(|src| doc.is_some_and(|doc| doc.selected_columns.contains(&src)));
            let header_name = source_col
                .and_then(|src| doc.map(|d| d.column_name(src)))
                .unwrap_or_default();
            let header_len =
                source_col.and_then(|src| doc.and_then(|d| d.header_value(src).map(<[u8]>::len)));
            (selected, header_name, header_len, tokens, show_all)
        };

        let (rendered, indicated, pending, tooltip_text) =
            if let (Some(src_col), Some(raw_len)) = (source_col, header_len) {
                let win_id = self.window_id;
                let summary = self.controller.update(cx, |controller, _| {
                    controller.inspect_header_whitespace(win_id, src_col, raw_len)
                });
                let controller = self.controller.read(cx);
                if let Some(raw) = controller
                    .active_document(win_id)
                    .and_then(|d| d.header_value(src_col))
                {
                    let rendered = render_whitespace(raw, summary, show_all, 120);
                    let indicated =
                        summary.is_some_and(|s| s.indicated(show_all)) || rendered.visible_marks;
                    let pending = summary.is_none() && !raw.is_empty();
                    let tip = (indicated || pending)
                        .then(|| controller.whitespace_explanation(win_id, summary));
                    (Some(rendered), indicated, pending, tip)
                } else {
                    (None, false, false, None)
                }
            } else {
                (None, false, false, None)
            };

        let mut header = div()
            .id(("grid-th", column))
            .relative()
            .size_full()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .cursor_pointer()
            .text_color(if selected {
                cx.theme().primary
            } else {
                cx.theme().foreground
            });

        if selected {
            header = header.child(div().flex_shrink_0().child("✓"));
        }

        let text_el = div().flex_1().min_w_0().overflow_hidden().text_ellipsis();
        header = if let Some(rendered) = rendered {
            header.child(
                text_el.child(rendered.into_styled_text(rgb(tokens.mark.to_rgb_hex()).into())),
            )
        } else {
            header.child(text_el.child(header_name))
        };

        if indicated {
            header = header.child(
                div()
                    .absolute()
                    .top(px(2.))
                    .right(px(2.))
                    .w(px(5.))
                    .h(px(5.))
                    .rounded_full()
                    .bg(rgb(tokens.indicator_fill.to_rgb_hex())),
            );
        } else if pending {
            header = header.child(
                div()
                    .absolute()
                    .top(px(2.))
                    .right(px(2.))
                    .w(px(5.))
                    .h(px(5.))
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(tokens.mark.to_rgb_hex())),
            );
        }

        if let Some(tip) = tooltip_text {
            header = header.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx));
        }

        if let Some(src_col) = source_col {
            let controller = self.controller.clone();
            let win_id = self.window_id;
            let on_action_left = self.on_action.clone();
            let on_action_right = self.on_action.clone();
            header = header
                .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    if event.click_count == 2 {
                        (on_action_left)(
                            GridAction::RenameHeader {
                                source_col: src_col,
                            },
                            window,
                            cx,
                        );
                        return;
                    }
                    let shift = event.modifiers.shift;
                    let toggle = event.modifiers.platform || event.modifiers.control;
                    controller.update(cx, |controller, cx| {
                        controller.select_column_active(win_id, src_col, shift, toggle, cx);
                    });
                })
                .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                    cx.stop_propagation();
                    (on_action_right)(
                        GridAction::ColumnContextMenu {
                            source_col: src_col,
                            position: event.position,
                        },
                        window,
                        cx,
                    );
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
        let (tokens, show_all) = {
            let controller = self.controller.read(cx);
            (
                controller.active_palette(),
                controller.show_all_whitespace(),
            )
        };
        let src_row_opt = self.source_row_at(row, cx);
        let src_col_opt = self.source_column_at(column, cx);
        let (row_selected, row_bookmarked, col_selected) = {
            let controller = self.controller.read(cx);
            let doc = controller.active_document(self.window_id);
            let row_sel =
                src_row_opt.is_some_and(|r| doc.is_some_and(|d| d.is_table_row_selected(row, r)));
            let row_bm = column == 0
                && src_row_opt.is_some_and(|r| doc.is_some_and(|d| d.bookmarks.contains_row(r)));
            let col_sel =
                src_col_opt.is_some_and(|c| doc.is_some_and(|d| d.selected_columns.contains(&c)));
            (row_sel, row_bm, col_sel)
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

        if column == 0 {
            if row_selected {
                cell = cell.bg(rgb(tokens.selection_strong.to_rgb_hex()));
            }
            if let Some(src_row) = src_row_opt {
                let controller = self.controller.clone();
                let win_id = self.window_id;
                let on_action_right = self.on_action.clone();
                let gutter_mouse_row = self.gutter_mouse_row.clone();
                cell = cell
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                        gutter_mouse_row.set(Some(row));
                        let shift = event.modifiers.shift;
                        let toggle = event.modifiers.platform || event.modifiers.control;
                        controller.update(cx, |controller, cx| {
                            controller.select_row_active(win_id, src_row, shift, toggle, cx);
                        });
                    })
                    .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                        cx.stop_propagation();
                        (on_action_right)(
                            GridAction::RowContextMenu {
                                table_row: row,
                                source_row: src_row,
                                position: event.position,
                            },
                            window,
                            cx,
                        );
                    });
            }
            if row_bookmarked {
                cell = cell.child(div().w(px(6.)).h(px(6.)).flex_shrink_0().rounded_full().bg(
                    rgb(if row_selected {
                        tokens.on_accent.to_rgb_hex()
                    } else {
                        tokens.accent_text.to_rgb_hex()
                    }),
                ));
            }
            let text = self.gutter_label(row, cx).unwrap_or_default();
            return cell.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_right()
                    .child(text),
            );
        }

        if row_selected || col_selected {
            cell = cell.bg(rgb(tokens.selection.to_rgb_hex()));
        }

        let cell_meta = {
            let controller = self.controller.read(cx);
            controller
                .active_document(self.window_id)
                .and_then(|doc| borrowed_cell_raw(doc, row, column))
                .map(|(src_row, src_col, raw)| (src_row, src_col, raw.map(<[u8]>::len)))
        };

        if let Some((src_row, src_col, raw_len_opt)) = cell_meta {
            let on_action_right = self.on_action.clone();
            cell = cell.on_mouse_down(MouseButton::Right, move |event, window, cx| {
                cx.stop_propagation();
                (on_action_right)(
                    GridAction::CellContextMenu {
                        table_row: row,
                        table_col: column,
                        source_row: src_row,
                        source_col: src_col,
                        position: event.position,
                    },
                    window,
                    cx,
                );
            });

            let Some(raw_len) = raw_len_opt else {
                // Absent/ragged field in this row: show clear hover tooltip distinguishing it from empty.
                let absent_tip = "This row has no cell in this file column.".to_owned();
                return cell
                    .tooltip(move |window, cx| Tooltip::new(absent_tip.clone()).build(window, cx))
                    .child(div().flex_1().min_w_0());
            };

            let win_id = self.window_id;
            let summary = self.controller.update(cx, |controller, _| {
                controller.inspect_cell_whitespace(win_id, src_row, src_col, raw_len)
            });

            let rendered_cell = {
                let controller = self.controller.read(cx);
                controller
                    .active_document(win_id)
                    .and_then(|doc| borrowed_cell_raw(doc, row, column))
                    .and_then(|(_, _, raw_opt)| raw_opt)
                    .map(|raw| {
                        let numeric_cell = source_looks_numeric(raw);
                        let rendered =
                            render_whitespace(raw, summary, show_all, MAX_GRID_TEXT_CHARS.min(240));
                        let invalid_utf8 =
                            summary.map_or(rendered.prefix_invalid_utf8, |s| s.invalid_utf8);
                        let indicated = summary.is_some_and(|s| s.indicated(show_all))
                            || rendered.visible_marks;
                        let pending = summary.is_none() && !raw.is_empty();
                        let ws_tip = (indicated || pending)
                            .then(|| controller.whitespace_explanation(win_id, summary));
                        (
                            numeric_cell,
                            rendered,
                            invalid_utf8,
                            indicated,
                            pending,
                            ws_tip,
                        )
                    })
            };

            let Some((numeric_cell, rendered, invalid_utf8, indicated, pending, ws_tip)) =
                rendered_cell
            else {
                return cell.child(div().flex_1().min_w_0());
            };

            let mut text_div = div().flex_1().min_w_0().overflow_hidden().text_ellipsis();
            if numeric_cell {
                text_div = text_div.text_right();
            }
            cell = cell.child(
                text_div.child(rendered.into_styled_text(rgb(tokens.mark.to_rgb_hex()).into())),
            );

            if indicated {
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
            } else if pending {
                cell = cell.child(
                    div()
                        .absolute()
                        .top(px(2.))
                        .right(px(2.))
                        .w(px(5.))
                        .h(px(5.))
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(tokens.mark.to_rgb_hex())),
                );
            }

            if invalid_utf8 || indicated || pending {
                let tip = match (invalid_utf8, ws_tip) {
                    (true, Some(ws)) => {
                        format!("This cell is not valid UTF-8 and cannot be edited.\n{ws}")
                    }
                    (true, None) => "This cell is not valid UTF-8 and cannot be edited.".to_owned(),
                    (false, Some(ws)) => ws,
                    (false, None) => unreachable!(),
                };
                cell = cell.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx));
            }
        } else {
            cell = cell.child(div().flex_1().min_w_0());
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
