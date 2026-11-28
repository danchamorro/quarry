//! Docked, keyboard-accessible Cell Inspector drawer backed by bounded lexers,
//! zero-copy shared cell inputs, and cancellable background line-start indexing /
//! JSON pretty-printing in `quarry_workbench::cell_inspector`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::component::{
    ActiveTheme, Disableable, Sizable,
    button::{Button, ButtonVariants},
};
use gpui_kit::{prelude::*, *};
use quarry_workbench::{
    CellFormat, CellInspectorSnapshot, DocumentId, GridSelection, MAX_COPY_BYTES,
    MAX_INSPECTOR_LINE_BYTES, PrettyJsonResult, WindowId, build_inspector_snapshot,
    inspector_token_color, tokenize_line,
};

use crate::menus::CopySelection;
use crate::workbench::WorkbenchController;

const INSPECTOR_LINE_HEIGHT_PX: f32 = 20.0;
const INSPECTOR_HPAN_STEP_CHARS: usize = 24;
const INSPECTOR_PAGE_LINES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorViewMode {
    Raw,
    Pretty,
}

pub enum CellInspectorEvent {
    Closed,
    EditCell { source_row: u64, source_col: usize },
    CopyText { text: String, label: &'static str },
    CopyError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectedTarget {
    doc_id: DocumentId,
    instance_id: u64,
    source_row: u64,
    source_col: usize,
    edit_revision: u64,
    display_row_number: u64,
    column_name: String,
    can_edit: bool,
}

pub struct CellInspector {
    controller: Entity<WorkbenchController>,
    window_id: WindowId,
    focus_handle: FocusHandle,
    open: bool,
    view_mode: InspectorViewMode,
    force_json: bool,
    target: Option<InspectedTarget>,
    absent_cell: bool,
    waiting_for_row: bool,
    generation: u64,
    active_cancel: Option<Arc<AtomicBool>>,
    active_task: Option<Task<()>>,
    snapshot: Option<CellInspectorSnapshot>,
    loading: bool,
    top_line: usize,
    h_offset_chars: usize,
    scroll_handle: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CellInspectorEvent> for CellInspector {}

impl CellInspector {
    pub fn new(
        controller: Entity<WorkbenchController>,
        window_id: WindowId,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let subscriptions = vec![cx.observe(&controller, |this, _, cx| {
            if this.open {
                this.sync_from_controller(cx);
            }
        })];
        Self {
            controller,
            window_id,
            focus_handle,
            open: false,
            view_mode: InspectorViewMode::Raw,
            force_json: false,
            target: None,
            absent_cell: false,
            waiting_for_row: false,
            generation: 0,
            active_cancel: None,
            active_task: None,
            snapshot: None,
            loading: false,
            top_line: 0,
            h_offset_chars: 0,
            scroll_handle: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    #[cfg(test)]
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn open_and_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let was_open = self.open;
        self.open = true;
        self.sync_from_controller(cx);
        self.focus_handle.focus(window, cx);
        if !was_open {
            cx.notify();
        }
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.close(cx);
        } else {
            self.open_and_focus(window, cx);
        }
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.cancel_in_flight();
        self.target = None;
        self.snapshot = None;
        self.absent_cell = false;
        self.waiting_for_row = false;
        self.loading = false;
        self.view_mode = InspectorViewMode::Raw;
        self.force_json = false;
        self.top_line = 0;
        self.h_offset_chars = 0;
        cx.emit(CellInspectorEvent::Closed);
        cx.notify();
    }

    pub fn clear_document(&mut self, cx: &mut Context<Self>) {
        self.cancel_in_flight();
        self.target = None;
        self.snapshot = None;
        self.absent_cell = false;
        self.waiting_for_row = false;
        self.loading = false;
        self.view_mode = InspectorViewMode::Raw;
        self.force_json = false;
        self.top_line = 0;
        self.h_offset_chars = 0;
        if self.open {
            self.open = false;
        }
        cx.notify();
    }

    fn cancel_in_flight(&mut self) {
        if let Some(flag) = self.active_cancel.take() {
            flag.store(true, Ordering::Relaxed);
        }
        self.active_task = None;
    }

    pub fn set_view_mode(&mut self, mode: InspectorViewMode, cx: &mut Context<Self>) {
        if self.view_mode == mode {
            return;
        }
        if mode == InspectorViewMode::Pretty && !self.has_ready_pretty_json() {
            return;
        }
        self.view_mode = mode;
        self.top_line = 0;
        self.h_offset_chars = 0;
        self.scroll_handle
            .scroll_to_item_strict(0, ScrollStrategy::Top);
        cx.notify();
    }

    pub fn toggle_force_json(&mut self, cx: &mut Context<Self>) {
        self.force_json = !self.force_json;
        self.view_mode = InspectorViewMode::Raw;
        self.top_line = 0;
        self.h_offset_chars = 0;
        self.start_job_for_current_target(cx);
    }

    fn has_ready_pretty_json(&self) -> bool {
        self.snapshot
            .as_ref()
            .and_then(|s| s.pretty_json.as_ref())
            .is_some_and(|p| matches!(p, PrettyJsonResult::Ready { .. }))
    }

    fn active_line_count(&self) -> usize {
        let Some(snap) = &self.snapshot else {
            return 0;
        };
        if self.view_mode == InspectorViewMode::Pretty
            && let Some(PrettyJsonResult::Ready { lines, .. }) = &snap.pretty_json
        {
            return lines.len();
        }
        snap.raw_lines.len()
    }

    fn scroll_vertical(&mut self, next_top: usize, cx: &mut Context<Self>) {
        let count = self.active_line_count();
        if count == 0 {
            return;
        }
        let clamped = next_top.min(count.saturating_sub(1));
        if clamped != self.top_line {
            self.top_line = clamped;
            self.scroll_handle
                .scroll_to_item_strict(clamped, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn pan_horizontal(&mut self, next_col: usize, cx: &mut Context<Self>) {
        let max_col = MAX_INSPECTOR_LINE_BYTES.saturating_sub(16);
        let clamped = next_col.min(max_col);
        if clamped != self.h_offset_chars {
            self.h_offset_chars = clamped;
            cx.notify();
        }
    }

    pub fn copy_current_view(&mut self, cx: &mut Context<Self>) {
        let Some(snap) = &self.snapshot else {
            return;
        };
        if self.view_mode == InspectorViewMode::Pretty
            && let Some(PrettyJsonResult::Ready { text, .. }) = &snap.pretty_json
        {
            if text.len() > MAX_COPY_BYTES {
                cx.emit(CellInspectorEvent::CopyError(
                    "Formatted JSON exceeds the 64 MiB clipboard limit.".into(),
                ));
                return;
            }
            cx.emit(CellInspectorEvent::CopyText {
                text: (**text).clone(),
                label: "Copied formatted JSON.",
            });
            return;
        }
        let raw_bytes = snap.raw.bytes();
        if raw_bytes.len() > MAX_COPY_BYTES {
            cx.emit(CellInspectorEvent::CopyError(
                "Cell exceeds the 64 MiB clipboard limit.".into(),
            ));
            return;
        }
        match std::str::from_utf8(raw_bytes) {
            Ok(text) => {
                cx.emit(CellInspectorEvent::CopyText {
                    text: text.to_owned(),
                    label: "Copied cell.",
                });
            }
            Err(_) => {
                cx.emit(CellInspectorEvent::CopyError(
                    "This cell contains invalid UTF-8 and cannot be copied as text.".into(),
                ));
            }
        }
    }

    pub fn sync_from_controller(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        // Step 1: Lightweight metadata-only lookup (ZERO cell byte allocations/copies!).
        let meta_lookup = {
            let controller = self.controller.read(cx);
            let active_doc_id = controller.scheduler().active_document(self.window_id);
            active_doc_id.and_then(|doc_id| {
                let doc = controller.active_document(self.window_id)?;
                let Some(GridSelection::Cell { row, column }) = doc.selection else {
                    return Some(None);
                };
                let display_row_number = doc
                    .global_data_row_number(row)
                    .unwrap_or_else(|| row.saturating_sub(doc.data_start).saturating_add(1));
                // Evaluate O(1) document-level edit guards here; cell presence and
                // UTF-8 validity come from the background snapshot so UI notifications
                // never scan multi-megabyte cell bytes on the main thread.
                let can_edit = doc.document_cell_edit_disabled_reason().is_none();
                Some(Some(InspectedTarget {
                    doc_id,
                    instance_id: doc.instance_id,
                    source_row: row,
                    source_col: column,
                    edit_revision: doc.edit_revision,
                    display_row_number,
                    column_name: doc.column_name(column),
                    can_edit,
                }))
            })
        };

        let Some(Some(next_target)) = meta_lookup else {
            if self.target.is_some()
                || self.snapshot.is_some()
                || self.loading
                || self.absent_cell
                || self.waiting_for_row
            {
                self.cancel_in_flight();
                self.target = None;
                self.snapshot = None;
                self.absent_cell = false;
                self.waiting_for_row = false;
                self.loading = false;
                self.view_mode = InspectorViewMode::Raw;
                self.force_json = false;
                self.top_line = 0;
                self.h_offset_chars = 0;
                cx.notify();
            }
            return;
        };

        let content_changed = self.target.as_ref().is_none_or(|prev| {
            prev.doc_id != next_target.doc_id
                || prev.instance_id != next_target.instance_id
                || prev.source_row != next_target.source_row
                || prev.source_col != next_target.source_col
                || prev.edit_revision != next_target.edit_revision
        });
        if !content_changed && !self.waiting_for_row {
            if self.target.as_ref() != Some(&next_target) {
                self.target = Some(next_target);
                cx.notify();
            }
            return;
        }

        if content_changed {
            self.force_json = false;
            self.view_mode = InspectorViewMode::Raw;
            self.top_line = 0;
            self.h_offset_chars = 0;
        }
        self.target = Some(next_target);
        self.start_job_for_current_target(cx);
    }

    fn start_job_for_current_target(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        // Step 2: Zero-copy Arc handle lookup (`Arc<Row>`, `Arc<FilterMatch>`, or `Arc<Vec<u8>>`).
        let (input_opt, row_loaded) = {
            let controller = self.controller.read(cx);
            controller
                .active_document(self.window_id)
                .map_or((None, false), |doc| {
                    let input = doc.cell_inspector_input(target.source_row, target.source_col);
                    let row_loaded = if doc.filter_active() {
                        doc.filtered_rows.iter().any(|m| m.row == target.source_row)
                    } else {
                        target.source_row >= doc.buffer_start
                            && target.source_row
                                < doc
                                    .buffer_start
                                    .saturating_add(doc.buffered_rows.len() as u64)
                    };
                    (input, row_loaded)
                })
        };

        self.cancel_in_flight();
        self.snapshot = None;
        self.scroll_handle
            .scroll_to_item_strict(0, ScrollStrategy::Top);

        let Some(raw) = input_opt else {
            if row_loaded {
                self.absent_cell = true;
                self.waiting_for_row = false;
                self.loading = false;
            } else {
                self.absent_cell = false;
                self.waiting_for_row = true;
                self.loading = true;
            }
            cx.notify();
            return;
        };

        self.absent_cell = false;
        self.waiting_for_row = false;
        self.loading = true;
        self.generation = self.generation.wrapping_add(1);
        let job_gen = self.generation;
        let force_json = self.force_json;
        let cancel = Arc::new(AtomicBool::new(false));
        self.active_cancel = Some(Arc::clone(&cancel));

        self.active_task = Some(cx.spawn(async move |this, cx| {
            let cancel_bg = Arc::clone(&cancel);
            let snapshot = cx
                .background_executor()
                .spawn(
                    async move { build_inspector_snapshot(job_gen, raw, force_json, &cancel_bg) },
                )
                .await;
            let Some(snapshot) = snapshot else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.open && this.generation == snapshot.generation {
                    this.snapshot = Some(snapshot);
                    this.loading = false;
                    this.active_task = None;
                    this.active_cancel = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn slice_line_with_pan(
        &self,
        line_idx: usize,
    ) -> Option<(String, Vec<quarry_workbench::TokenSpan>, usize, usize, bool)> {
        let snap = self.snapshot.as_ref()?;
        let (bytes, line_index, format): (&[u8], &quarry_workbench::LineIndex, CellFormat) =
            if self.view_mode == InspectorViewMode::Pretty
                && let Some(PrettyJsonResult::Ready { text, lines }) = &snap.pretty_json
            {
                (text.as_bytes(), lines, CellFormat::Json)
            } else {
                (snap.raw.bytes(), &snap.raw_lines, snap.detected_format)
            };

        let in_fence = format == CellFormat::Markdown && line_index.in_code_fence_before(line_idx);
        let slice = line_index.line_slice(bytes, line_idx, MAX_INSPECTOR_LINE_BYTES)?;
        let tokenized = tokenize_line(&slice.text, format, in_fence, MAX_INSPECTOR_LINE_BYTES);
        let total_truncated = slice
            .truncated_bytes
            .saturating_add(tokenized.truncated_bytes);

        let pan_byte_start = tokenized
            .text
            .char_indices()
            .nth(self.h_offset_chars)
            .map_or(tokenized.text.len(), |(idx, _)| idx);
        let visible_text = tokenized.text[pan_byte_start..].to_owned();
        let spans = tokenized
            .spans
            .into_iter()
            .filter_map(|span| {
                if span.range.end <= pan_byte_start {
                    return None;
                }
                let start = span.range.start.saturating_sub(pan_byte_start);
                let end = span.range.end.saturating_sub(pan_byte_start);
                (start < end && end <= visible_text.len()).then_some(quarry_workbench::TokenSpan {
                    range: start..end,
                    kind: span.kind,
                })
            })
            .collect();
        let overflows_drawer = visible_text.chars().count() > 44;

        Some((
            visible_text,
            spans,
            total_truncated,
            slice.total_line_bytes,
            overflows_drawer,
        ))
    }

    fn render_line_row(&self, line_idx: usize, cx: &App) -> AnyElement {
        let resolved = self.controller.read(cx).resolved_theme();
        let tokens = resolved.palette;
        let appearance = resolved.appearance;
        let Some((mut text_content, spans, total_truncated, _, overflows_drawer)) =
            self.slice_line_with_pan(line_idx)
        else {
            return div().h(px(INSPECTOR_LINE_HEIGHT_PX)).into_any_element();
        };

        let highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = spans
            .into_iter()
            .map(|span| {
                let color = inspector_token_color(span.kind, appearance, &tokens);
                (
                    span.range,
                    HighlightStyle {
                        color: Some(rgb(color.to_rgb_hex()).into()),
                        ..HighlightStyle::default()
                    },
                )
            })
            .collect();

        if text_content.is_empty() {
            text_content.push(' ');
        }

        let styled = if highlights.is_empty() {
            StyledText::new(text_content)
        } else {
            StyledText::new(text_content).with_highlights(highlights)
        };

        let mut row = div()
            .h(px(INSPECTOR_LINE_HEIGHT_PX))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .text_size(px(12.))
            .font_family(cx.theme().mono_font_family.clone())
            .child(
                div()
                    .w(px(44.))
                    .flex_shrink_0()
                    .text_right()
                    .text_color(rgb(tokens.text_secondary.to_rgb_hex()))
                    .text_size(px(11.))
                    .child((line_idx + 1).to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(rgb(tokens.text.to_rgb_hex()))
                    .child(styled),
            );

        if total_truncated > 0 {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(tokens.warning_text.to_rgb_hex()))
                    .child("…"),
            );
        } else if overflows_drawer {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(tokens.text_secondary.to_rgb_hex()))
                    .child("→"),
            );
        }

        row.into_any_element()
    }

    #[cfg(test)]
    pub fn is_loading_for_test(&self) -> bool {
        self.loading
    }

    #[cfg(test)]
    pub fn generation_for_test(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub fn top_line_for_test(&self) -> usize {
        self.top_line
    }

    #[cfg(test)]
    pub fn scroll_offset_y_for_test(&self) -> f32 {
        -f32::from(self.scroll_handle.0.borrow().base_handle.offset().y)
    }

    #[cfg(test)]
    pub fn h_offset_chars_for_test(&self) -> usize {
        self.h_offset_chars
    }

    #[cfg(test)]
    pub fn detected_format_for_test(&self) -> Option<CellFormat> {
        self.snapshot.as_ref().map(|s| s.detected_format)
    }

    #[cfg(test)]
    pub fn view_mode_for_test(&self) -> InspectorViewMode {
        self.view_mode
    }

    #[cfg(test)]
    pub fn pretty_json_error_for_test(&self) -> Option<String> {
        self.snapshot
            .as_ref()
            .and_then(|s| s.pretty_json.as_ref())
            .and_then(|p| match p {
                PrettyJsonResult::Invalid(err) => Some(err.display_summary()),
                _ => None,
            })
    }

    #[cfg(test)]
    pub fn line_count_for_test(&self) -> usize {
        self.active_line_count()
    }

    #[cfg(test)]
    pub fn line_preview_for_test(&self, line_idx: usize) -> Option<(String, usize)> {
        let (text, _, truncated, _, _) = self.slice_line_with_pan(line_idx)?;
        Some((text, truncated))
    }

    #[cfg(test)]
    pub fn inspected_coord_for_test(&self) -> Option<(u64, usize)> {
        self.target.as_ref().map(|t| (t.source_row, t.source_col))
    }
}

impl Render for CellInspector {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.controller.read(cx).active_palette();
        let target = self.target.clone();
        let can_copy = self
            .snapshot
            .as_ref()
            .is_some_and(|s| s.valid_utf8 || self.has_ready_pretty_json());
        let can_edit = target.as_ref().is_some_and(|t| t.can_edit)
            && !self.absent_cell
            && self.snapshot.as_ref().is_some_and(|s| s.valid_utf8);
        let is_json = self
            .snapshot
            .as_ref()
            .is_some_and(|s| s.detected_format == CellFormat::Json)
            || self.force_json;
        let has_pretty = self.has_ready_pretty_json();

        let (format_label, stats_label) = if let Some(snap) = &self.snapshot {
            let active_lines = if self.view_mode == InspectorViewMode::Pretty
                && let Some(PrettyJsonResult::Ready { lines, .. }) = &snap.pretty_json
            {
                lines
            } else {
                &snap.raw_lines
            };
            let line_count = active_lines.len();
            let truncated_note = if active_lines.truncated_lines() {
                "+"
            } else {
                ""
            };
            (
                Some(snap.detected_format.label()),
                Some(format!(
                    "{} · {}{} {}",
                    format_byte_count(snap.raw.bytes().len()),
                    line_count,
                    truncated_note,
                    if line_count == 1 { "line" } else { "lines" }
                )),
            )
        } else {
            (None, None)
        };

        let json_notice: Option<(bool, String)> =
            self.snapshot
                .as_ref()
                .and_then(|s| match s.pretty_json.as_ref() {
                    Some(PrettyJsonResult::Invalid(err)) => Some((true, err.display_summary())),
                    Some(PrettyJsonResult::DepthLimitExceeded {
                        max_depth,
                        line,
                        column,
                        ..
                    }) => Some((
                        false,
                        format!(
                            "JSON nesting exceeds {max_depth}-level limit (line {line}, column {column}); showing bounded raw view."
                        ),
                    )),
                    Some(PrettyJsonResult::TooLarge { limit_bytes, .. }) => Some((
                        false,
                        format!(
                            "JSON exceeds {} pretty-print limit; showing bounded raw view.",
                            format_byte_count(*limit_bytes)
                        ),
                    )),
                    _ => None,
                });

        let invalid_utf8 = self.snapshot.as_ref().is_some_and(|s| !s.valid_utf8);

        let mut drawer = div()
            .id("cell-inspector-drawer")
            .track_focus(&self.focus_handle)
            .role(Role::Region)
            .aria_label("Cell Inspector")
            .w(px(400.))
            .min_w(px(280.))
            .max_w(px(440.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(tokens.canvas.to_rgb_hex()))
            .border_l_1()
            .border_color(rgb(tokens.separator.to_rgb_hex()))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                this.copy_current_view(cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let mods = event.keystroke.modifiers;
                match event.keystroke.key.as_str() {
                    "escape" => {
                        cx.stop_propagation();
                        this.close(cx);
                    }
                    "down" => {
                        cx.stop_propagation();
                        if mods.platform || mods.control {
                            let end = this.active_line_count().saturating_sub(1);
                            this.scroll_vertical(end, cx);
                        } else {
                            this.scroll_vertical(this.top_line.saturating_add(1), cx);
                        }
                    }
                    "up" => {
                        cx.stop_propagation();
                        if mods.platform || mods.control {
                            this.scroll_vertical(0, cx);
                        } else {
                            this.scroll_vertical(this.top_line.saturating_sub(1), cx);
                        }
                    }
                    "pagedown" => {
                        cx.stop_propagation();
                        this.scroll_vertical(
                            this.top_line.saturating_add(INSPECTOR_PAGE_LINES),
                            cx,
                        );
                    }
                    "pageup" => {
                        cx.stop_propagation();
                        this.scroll_vertical(
                            this.top_line.saturating_sub(INSPECTOR_PAGE_LINES),
                            cx,
                        );
                    }
                    "right" => {
                        cx.stop_propagation();
                        this.pan_horizontal(
                            this.h_offset_chars
                                .saturating_add(INSPECTOR_HPAN_STEP_CHARS),
                            cx,
                        );
                    }
                    "left" => {
                        cx.stop_propagation();
                        this.pan_horizontal(
                            this.h_offset_chars
                                .saturating_sub(INSPECTOR_HPAN_STEP_CHARS),
                            cx,
                        );
                    }
                    "home" => {
                        cx.stop_propagation();
                        this.pan_horizontal(0, cx);
                    }
                    "end" => {
                        cx.stop_propagation();
                        if let Some((text, _, _, _, _)) = this.slice_line_with_pan(this.top_line) {
                            let total_chars = self_line_chars(this.h_offset_chars, &text);
                            this.pan_horizontal(total_chars.saturating_sub(32), cx);
                        }
                    }
                    _ => {}
                }
            }));

        // Header
        let subtitle = target
            .as_ref()
            .map(|t| format!("Row {} · {}", t.display_row_number, t.column_name))
            .unwrap_or_else(|| "No cell selected".into());

        drawer = drawer.child(
            div()
                .px_3()
                .py_2()
                .bg(rgb(tokens.chrome.to_rgb_hex()))
                .border_b_1()
                .border_color(rgb(tokens.separator.to_rgb_hex()))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(tokens.text.to_rgb_hex()))
                                        .child("Cell Inspector"),
                                )
                                .when_some(format_label, |el, label| {
                                    el.child(
                                        div()
                                            .id("cell-inspector-format-badge")
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(px(4.))
                                            .bg(rgb(tokens.hover.to_rgb_hex()))
                                            .text_size(px(11.))
                                            .text_color(rgb(tokens.text_secondary.to_rgb_hex()))
                                            .child(label),
                                    )
                                }),
                        )
                        .child(
                            Button::new("cell-inspector-close")
                                .label("×")
                                .accessibility_label("Close Cell Inspector")
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close(cx);
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .text_size(px(11.))
                        .text_color(rgb(tokens.text_secondary.to_rgb_hex()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(subtitle),
                        )
                        .when_some(stats_label, |el, stats| {
                            el.child(div().flex_shrink_0().child(stats))
                        }),
                ),
        );

        // Action strip
        let mut actions = div()
            .px_2()
            .py_1p5()
            .bg(rgb(tokens.chrome.to_rgb_hex()))
            .border_b_1()
            .border_color(rgb(tokens.separator.to_rgb_hex()))
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_1();

        if is_json {
            let raw_btn = Button::new("cell-inspector-raw")
                .label("Raw")
                .xsmall()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_view_mode(InspectorViewMode::Raw, cx);
                }));
            let raw_btn = if self.view_mode == InspectorViewMode::Raw {
                raw_btn.primary()
            } else {
                raw_btn.ghost()
            };

            let pretty_btn = Button::new("cell-inspector-pretty")
                .label("Pretty JSON")
                .xsmall()
                .disabled(!has_pretty)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.set_view_mode(InspectorViewMode::Pretty, cx);
                }));
            let pretty_btn = if self.view_mode == InspectorViewMode::Pretty {
                pretty_btn.primary()
            } else {
                pretty_btn.ghost()
            };

            actions = actions.child(raw_btn).child(pretty_btn);
        } else if self.snapshot.is_some() {
            actions = actions.child(
                Button::new("cell-inspector-try-json")
                    .label("Parse JSON")
                    .ghost()
                    .xsmall()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_force_json(cx);
                    })),
            );
        }

        if self.snapshot.is_some() {
            actions = actions
                .child(
                    Button::new("cell-inspector-pan-left")
                        .label("◀")
                        .accessibility_label("Pan line left")
                        .ghost()
                        .xsmall()
                        .disabled(self.h_offset_chars == 0)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.pan_horizontal(
                                this.h_offset_chars
                                    .saturating_sub(INSPECTOR_HPAN_STEP_CHARS),
                                cx,
                            );
                        })),
                )
                .child(
                    Button::new("cell-inspector-pan-right")
                        .label("▶")
                        .accessibility_label("Pan line right")
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.pan_horizontal(
                                this.h_offset_chars
                                    .saturating_add(INSPECTOR_HPAN_STEP_CHARS),
                                cx,
                            );
                        })),
                );
            if self.h_offset_chars > 0 {
                actions = actions.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(tokens.text_secondary.to_rgb_hex()))
                        .child(format!("Col {}", self.h_offset_chars + 1)),
                );
            }
        }

        let copy_tooltip = if self.view_mode == InspectorViewMode::Pretty && has_pretty {
            "Copy formatted JSON"
        } else {
            "Copy raw cell"
        };
        actions = actions
            .child(div().flex_1())
            .child(
                Button::new("cell-inspector-copy")
                    .label("Copy")
                    .accessibility_label(copy_tooltip)
                    .tooltip(copy_tooltip)
                    .ghost()
                    .xsmall()
                    .disabled(!can_copy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.copy_current_view(cx);
                    })),
            )
            .child(
                Button::new("cell-inspector-edit")
                    .label("Edit…")
                    .ghost()
                    .xsmall()
                    .disabled(!can_edit)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(t) = &this.target {
                            cx.emit(CellInspectorEvent::EditCell {
                                source_row: t.source_row,
                                source_col: t.source_col,
                            });
                        }
                    })),
            );
        drawer = drawer.child(actions);

        // Diagnostic banners (invalid JSON error position, nesting/size caps, line budget, invalid UTF-8)
        let long_line_notice: Option<String> = self.snapshot.as_ref().and_then(|snap| {
            let active_lines = if self.view_mode == InspectorViewMode::Pretty
                && let Some(PrettyJsonResult::Ready { lines, .. }) = &snap.pretty_json
            {
                lines
            } else {
                &snap.raw_lines
            };
            (active_lines.max_line_bytes() > MAX_INSPECTOR_LINE_BYTES).then(|| {
                format!(
                    "Long line capped at {} per line (longest line: {}); use Copy for full text.",
                    format_byte_count(MAX_INSPECTOR_LINE_BYTES),
                    format_byte_count(active_lines.max_line_bytes()),
                )
            })
        });

        if let Some(notice) = long_line_notice {
            drawer = drawer.child(
                div()
                    .id("cell-inspector-line-budget-notice")
                    .role(Role::Status)
                    .px_3()
                    .py_1p5()
                    .bg(rgb(tokens.warning_fill.to_rgb_hex()))
                    .border_b_1()
                    .border_color(rgb(tokens.separator.to_rgb_hex()))
                    .text_size(px(11.))
                    .text_color(rgb(tokens.warning_text.to_rgb_hex()))
                    .child(notice),
            );
        }
        if let Some((is_error, notice)) = json_notice {
            let (bg, fg) = if is_error {
                (tokens.error_fill, tokens.error_text)
            } else {
                (tokens.warning_fill, tokens.warning_text)
            };
            drawer = drawer.child(
                div()
                    .id("cell-inspector-json-notice")
                    .role(Role::Alert)
                    .px_3()
                    .py_1p5()
                    .bg(rgb(bg.to_rgb_hex()))
                    .border_b_1()
                    .border_color(rgb(tokens.separator.to_rgb_hex()))
                    .text_size(px(11.))
                    .text_color(rgb(fg.to_rgb_hex()))
                    .child(notice),
            );
        }

        if invalid_utf8 {
            drawer = drawer.child(
                div()
                    .id("cell-inspector-utf8-notice")
                    .role(Role::Status)
                    .px_3()
                    .py_1p5()
                    .bg(rgb(tokens.warning_fill.to_rgb_hex()))
                    .border_b_1()
                    .border_color(rgb(tokens.separator.to_rgb_hex()))
                    .text_size(px(11.))
                    .text_color(rgb(tokens.warning_text.to_rgb_hex()))
                    .child("This cell contains invalid UTF-8 bytes; displaying lossy preview."),
            );
        }

        // Body
        if self.target.is_none() {
            drawer = drawer.child(
                div()
                    .flex_1()
                    .p_4()
                    .text_size(px(12.))
                    .text_color(rgb(tokens.text_secondary.to_rgb_hex()))
                    .child("Select a cell to inspect its contents."),
            );
        } else if self.absent_cell {
            drawer = drawer.child(
                div()
                    .flex_1()
                    .p_4()
                    .text_size(px(12.))
                    .text_color(rgb(tokens.text_secondary.to_rgb_hex()))
                    .child("This row has no cell in this file column."),
            );
        } else if self.loading || self.snapshot.is_none() {
            drawer = drawer.child(
                div()
                    .flex_1()
                    .p_4()
                    .text_size(px(12.))
                    .text_color(rgb(tokens.text_secondary.to_rgb_hex()))
                    .child("Indexing cell…"),
            );
        } else {
            let line_count = self.active_line_count();
            let entity = cx.entity().clone();
            drawer = drawer.child(
                div()
                    .id("cell-inspector-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .py_1()
                    .child(
                        uniform_list("cell-inspector-lines", line_count, move |range, _, cx| {
                            let view = entity.read(cx);
                            range
                                .map(|idx| view.render_line_row(idx, cx))
                                .collect::<Vec<_>>()
                        })
                        .track_scroll(&self.scroll_handle)
                        .size_full(),
                    ),
            );
        }

        drawer
    }
}

fn self_line_chars(offset: usize, visible_tail: &str) -> usize {
    offset.saturating_add(visible_tail.chars().count())
}

fn format_byte_count(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if (bytes as f64) < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}
