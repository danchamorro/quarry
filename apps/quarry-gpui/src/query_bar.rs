//! Inline Filter Query Bar backed by `WorkbenchController` and `quarry_core::filter_expr`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme, Disableable, Sizable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{prelude::*, *};
use quarry_core::{CaseSensitivity, FilterExprError, FilterQuery};
use quarry_workbench::{DocumentId, WindowId};

use crate::workbench::{DocumentSnapshot, WorkbenchController};

/// Debounce delay for opt-in live as-you-type filtering.
const LIVE_FILTER_DEBOUNCE: Duration = Duration::from_millis(180);

pub enum QueryBarEvent {
    Apply(FilterQuery),
    Clear,
    OpenRules,
    SwitchToFind,
    Closed,
}

#[derive(Clone, Debug, Default)]
struct QueryTabDraft {
    text: String,
    match_case: bool,
    live_override: Option<bool>,
    draft_dirty: bool,
    last_synced_query: Option<FilterQuery>,
    last_synced_headers: Vec<String>,
    last_synced_expression: Option<Result<String, String>>,
    print_omitted_reason: Option<String>,
}

pub struct QueryBar {
    controller: Entity<WorkbenchController>,
    window_id: WindowId,
    open: bool,
    active_doc: Option<(DocumentId, u64)>,
    input: Entity<InputState>,
    match_case: bool,
    live_mode: bool,
    live_override: Option<bool>,
    draft_dirty: bool,
    pending_applied_query: Option<FilterQuery>,
    last_synced_query: Option<FilterQuery>,
    last_synced_headers: Vec<String>,
    last_synced_expression: Option<Result<String, String>>,
    draft_generation: u64,
    suppress_change: bool,
    parse_error: Option<FilterExprError>,
    print_omitted_reason: Option<String>,
    tab_drafts: HashMap<DocumentId, QueryTabDraft>,
    debounce_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<QueryBarEvent> for QueryBar {}

impl QueryBar {
    pub fn new(
        controller: Entity<WorkbenchController>,
        window_id: WindowId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter expression (e.g. state == \"TX\" AND amount >= 100)")
        });
        let subscriptions =
            vec![
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::PressEnter { .. } => {
                            this.apply_now(cx);
                        }
                        InputEvent::Change if !this.suppress_change => {
                            this.on_input_changed(window, cx);
                        }
                        _ => {}
                    }
                }),
            ];
        Self {
            controller,
            window_id,
            open: false,
            active_doc: None,
            input,
            match_case: false,
            live_mode: true,
            live_override: None,
            draft_dirty: false,
            pending_applied_query: None,
            last_synced_query: None,
            last_synced_headers: Vec::new(),
            last_synced_expression: None,
            draft_generation: 0,
            suppress_change: false,
            parse_error: None,
            print_omitted_reason: None,
            tab_drafts: HashMap::new(),
            debounce_task: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open_and_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.revalidate_current(cx);
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.close(cx);
        } else {
            self.open_and_focus(window, cx);
        }
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            self.debounce_task = None;
            cx.emit(QueryBarEvent::Closed);
            cx.notify();
        }
    }

    pub fn close_silent(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            self.debounce_task = None;
            cx.notify();
        }
    }

    pub fn cancel_pending_apply_sync(&mut self) {
        self.pending_applied_query = None;
    }

    pub fn clear_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active_doc = None;
        self.tab_drafts.clear();
        self.debounce_task = None;
        self.parse_error = None;
        self.print_omitted_reason = None;
        self.live_override = None;
        self.live_mode = true;
        self.draft_dirty = false;
        self.pending_applied_query = None;
        self.last_synced_query = None;
        self.last_synced_headers.clear();
        self.last_synced_expression = None;
        self.set_input_silently("", window, cx);
        self.open = false;
        cx.notify();
    }

    pub fn sync_document(
        &mut self,
        info: &DocumentSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open_tabs: HashSet<DocumentId> = self
            .controller
            .read(cx)
            .tabs_for_window(self.window_id)
            .into_iter()
            .map(|tab| tab.id)
            .collect();
        let doc_key = (info.document_id, info.instance_id);
        let doc_changed = self.active_doc != Some(doc_key);
        if doc_changed {
            if let Some((prev_id, _)) = self.active_doc
                && open_tabs.contains(&prev_id)
            {
                self.save_active_draft(cx);
            }
            self.debounce_task = None;
            self.pending_applied_query = None;
            self.active_doc = Some(doc_key);
            if let Some(saved) = self.tab_drafts.get(&info.document_id).cloned() {
                self.live_override = saved.live_override;
                self.live_mode = saved
                    .live_override
                    .unwrap_or(info.default_live_filter_enabled);
                self.match_case = saved.match_case;
                self.draft_dirty = saved.draft_dirty;
                self.last_synced_query = saved.last_synced_query;
                self.last_synced_headers = saved.last_synced_headers;
                self.last_synced_expression = saved.last_synced_expression;
                self.print_omitted_reason = saved.print_omitted_reason;
                self.set_input_silently(&saved.text, window, cx);
            } else {
                self.live_override = None;
                self.live_mode = info.default_live_filter_enabled;
                self.match_case = info
                    .filter_query
                    .as_ref()
                    .is_some_and(|q| q.case_sensitivity == CaseSensitivity::Sensitive);
                self.draft_dirty = false;
                self.last_synced_query = None;
                self.last_synced_headers = info.headers.clone();
                self.last_synced_expression = None;
                self.print_omitted_reason = None;
                self.set_input_silently("", window, cx);
            }
        } else if self.live_override.is_none() {
            self.live_mode = info.default_live_filter_enabled;
        }
        self.tab_drafts.retain(|id, _| open_tabs.contains(id));

        // Track applied query changes, header renames (including changes past the 120-char
        // display-header truncation via `filter_expression`), and dirty drafts independently of
        // `filter_query_generation` so cell-edit rescans never overwrite an unapplied draft
        // while header renames refresh clean expressions and revalidate dirty drafts.
        let query_changed = self.last_synced_query != info.filter_query;
        let headers_changed = self.last_synced_headers != info.headers
            || self.last_synced_expression != info.filter_expression;
        let force_apply_sync = self.pending_applied_query.is_some()
            && info.filter_query.as_ref() == self.pending_applied_query.as_ref();
        if doc_changed || query_changed || headers_changed || force_apply_sync {
            self.last_synced_query = info.filter_query.clone();
            self.last_synced_headers = info.headers.clone();
            self.last_synced_expression = info.filter_expression.clone();
            if force_apply_sync {
                self.pending_applied_query = None;
            }
            let should_sync_expression = if query_changed || force_apply_sync {
                true
            } else if headers_changed || doc_changed {
                !self.draft_dirty && info.filter_query.is_some()
            } else {
                false
            };
            if should_sync_expression {
                self.debounce_task = None;
                self.print_omitted_reason = None;
                match &info.filter_expression {
                    Some(Ok(canonical)) => {
                        self.match_case = info
                            .filter_query
                            .as_ref()
                            .is_some_and(|q| q.case_sensitivity == CaseSensitivity::Sensitive);
                        self.parse_error = None;
                        self.draft_dirty = false;
                        self.set_input_silently(canonical, window, cx);
                    }
                    Some(Err(reason)) => {
                        self.match_case = info
                            .filter_query
                            .as_ref()
                            .is_some_and(|q| q.case_sensitivity == CaseSensitivity::Sensitive);
                        self.parse_error = None;
                        self.draft_dirty = false;
                        self.print_omitted_reason = Some(reason.clone());
                        self.set_input_silently("", window, cx);
                    }
                    None => {
                        if query_changed {
                            self.draft_dirty = false;
                            self.parse_error = None;
                            self.set_input_silently("", window, cx);
                        }
                    }
                }
            }
            self.revalidate_current(cx);
        }
        cx.notify();
    }

    fn save_active_draft(&mut self, cx: &App) {
        if let Some((doc_id, _)) = self.active_doc {
            self.tab_drafts.insert(
                doc_id,
                QueryTabDraft {
                    text: self.input.read(cx).value().to_string(),
                    match_case: self.match_case,
                    live_override: self.live_override,
                    draft_dirty: self.draft_dirty,
                    last_synced_query: self.last_synced_query.clone(),
                    last_synced_headers: self.last_synced_headers.clone(),
                    last_synced_expression: self.last_synced_expression.clone(),
                    print_omitted_reason: self.print_omitted_reason.clone(),
                },
            );
        }
    }

    fn set_input_silently(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.read(cx).value().as_str() == text {
            return;
        }
        self.suppress_change = true;
        self.input
            .update(cx, |state, cx| state.set_value(text.to_owned(), window, cx));
        self.suppress_change = false;
    }

    fn case_sensitivity(&self) -> CaseSensitivity {
        if self.match_case {
            CaseSensitivity::Sensitive
        } else {
            CaseSensitivity::Insensitive
        }
    }

    fn revalidate_current(&mut self, cx: &App) -> Option<Result<FilterQuery, FilterExprError>> {
        let text = self.input.read(cx).value().to_string();
        if text.trim().is_empty() {
            self.parse_error = None;
            return None;
        }
        let case = self.case_sensitivity();
        let win = self.window_id;
        let result = self
            .controller
            .read(cx)
            .parse_filter_expression_active(win, &text, case);
        match &result {
            Some(Ok(_)) => {
                self.parse_error = None;
            }
            Some(Err(err)) => {
                self.parse_error = Some(err.clone());
            }
            None => {
                self.parse_error = None;
            }
        }
        result
    }

    fn on_input_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.draft_generation = self.draft_generation.wrapping_add(1);
        self.draft_dirty = true;
        self.debounce_task = None;
        self.print_omitted_reason = None;

        let validated = self.revalidate_current(cx);
        if self.live_mode {
            match validated {
                Some(Ok(query)) => self.schedule_live_apply(Some(query), window, cx),
                Some(Err(_)) => {
                    // Invalid or unrepresentable expression: keep the active filter untouched.
                }
                None => {
                    let has_active_filter = self
                        .controller
                        .read(cx)
                        .active_snapshot(self.window_id)
                        .is_some_and(|s| s.filter_active);
                    if has_active_filter {
                        self.schedule_live_apply(None, window, cx);
                    }
                }
            }
        }
        cx.notify();
    }

    fn schedule_live_apply(
        &mut self,
        query: Option<FilterQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(info) = self.controller.read(cx).active_snapshot(self.window_id) else {
            return;
        };
        if info.busy || info.export_running || info.search_running || info.source_changed {
            return;
        }
        let target_doc = (info.document_id, info.instance_id);
        let target_edit_rev = info.edit_revision;
        let target_query_gen = info.filter_query_generation;
        let target_draft = self.draft_generation;

        self.debounce_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(LIVE_FILTER_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if !this.live_mode || this.draft_generation != target_draft {
                    return;
                }
                let Some(current) = this.controller.read(cx).active_snapshot(this.window_id) else {
                    return;
                };
                if (current.document_id, current.instance_id) != target_doc
                    || current.edit_revision != target_edit_rev
                    || current.filter_query_generation != target_query_gen
                    || current.busy
                    || current.export_running
                    || current.search_running
                    || current.source_changed
                {
                    return;
                }
                match query {
                    Some(q) => {
                        // Reparse against current headers before emitting so a header rename
                        // during the debounce window cannot apply a stale column mapping.
                        match this.revalidate_current(cx) {
                            Some(Ok(reparsed)) if reparsed == q => {
                                if current.filter_query.as_ref() != Some(&reparsed) {
                                    this.pending_applied_query = Some(reparsed.clone());
                                    cx.emit(QueryBarEvent::Apply(reparsed));
                                }
                            }
                            _ => {
                                cx.notify();
                            }
                        }
                    }
                    None => {
                        if this.input.read(cx).value().trim().is_empty()
                            && this.print_omitted_reason.is_none()
                            && current.filter_active
                        {
                            cx.emit(QueryBarEvent::Clear);
                        }
                    }
                }
            });
        }));
    }

    pub fn apply_now(&mut self, cx: &mut Context<Self>) {
        self.debounce_task = None;
        let Some(info) = self.controller.read(cx).active_snapshot(self.window_id) else {
            return;
        };
        if info.busy || info.export_running || info.search_running || info.source_changed {
            return;
        }
        match self.revalidate_current(cx) {
            Some(Ok(query)) => {
                self.pending_applied_query = Some(query.clone());
                cx.emit(QueryBarEvent::Apply(query));
            }
            Some(Err(_)) => {
                // Keep inline diagnostic visible and leave any active filter undisturbed.
            }
            None => {
                // Do not clear an active filter on bare Enter while its expression is omitted
                // from the text field due to printer limits; require explicit Clear or new input.
                if self.print_omitted_reason.is_some() {
                    return;
                }
                if info.filter_active {
                    cx.emit(QueryBarEvent::Clear);
                }
            }
        }
        cx.notify();
    }

    pub fn clear_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(info) = self.controller.read(cx).active_snapshot(self.window_id) else {
            return;
        };
        if info.busy || info.export_running || info.search_running || info.source_changed {
            return;
        }
        self.debounce_task = None;
        self.draft_dirty = false;
        self.pending_applied_query = None;
        self.parse_error = None;
        self.print_omitted_reason = None;
        self.set_input_silently("", window, cx);
        self.input.update(cx, |input, cx| input.focus(window, cx));
        if info.filter_active {
            cx.emit(QueryBarEvent::Clear);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn set_expression_for_test(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input
            .update(cx, |state, cx| state.set_value(text.to_owned(), window, cx));
        self.on_input_changed(window, cx);
    }

    fn set_live_mode(&mut self, live: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.live_mode = live;
        self.live_override = Some(live);
        self.debounce_task = None;
        if live {
            let text_empty = self.input.read(cx).value().trim().is_empty();
            if text_empty && self.print_omitted_reason.is_some() {
                cx.notify();
                return;
            }
            match self.revalidate_current(cx) {
                Some(Ok(query)) => {
                    self.draft_generation = self.draft_generation.wrapping_add(1);
                    self.schedule_live_apply(Some(query), window, cx);
                }
                Some(Err(_)) => {}
                None if self.draft_dirty => {
                    let has_active_filter = self
                        .controller
                        .read(cx)
                        .active_snapshot(self.window_id)
                        .is_some_and(|s| s.filter_active);
                    if has_active_filter {
                        self.draft_generation = self.draft_generation.wrapping_add(1);
                        self.schedule_live_apply(None, window, cx);
                    }
                }
                None => {}
            }
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn set_live_mode_for_test(
        &mut self,
        live: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_live_mode(live, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn set_match_case_for_test(&mut self, match_case: bool, cx: &mut Context<Self>) {
        self.match_case = match_case;
        self.revalidate_current(cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn text_for_test(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn parse_error_for_test(&self) -> Option<&FilterExprError> {
        self.parse_error.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn live_mode_for_test(&self) -> bool {
        self.live_mode
    }

    #[cfg(test)]
    pub(crate) fn tab_draft_count_for_test(&self) -> usize {
        self.tab_drafts.len()
    }

    #[cfg(test)]
    pub(crate) fn print_omitted_reason_for_test(&self) -> Option<&str> {
        self.print_omitted_reason.as_deref()
    }
}

fn format_inline_diagnostic(text: &str, err: &FilterExprError) -> String {
    let snippet = text
        .get(err.span.start..err.span.end)
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 24);
    if let Some(snippet) = snippet {
        format!(
            "Byte {} (near `{snippet}`): {}",
            err.span.start, err.message
        )
    } else {
        format!("Byte {}: {}", err.span.start, err.message)
    }
}

impl Render for QueryBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let Some(info) = self.controller.read(cx).active_snapshot(self.window_id) else {
            return div().into_any_element();
        };
        let text = self.input.read(cx).value().to_string();
        let has_text = !text.trim().is_empty();
        let blocked =
            info.busy || info.export_running || info.search_running || info.source_changed;
        let can_apply = !blocked
            && self.parse_error.is_none()
            && (has_text || (info.filter_active && self.print_omitted_reason.is_none()));
        let can_clear = !blocked && (has_text || info.filter_active);
        let diagnostic = self
            .parse_error
            .as_ref()
            .map(|err| format_inline_diagnostic(&text, err));
        let omitted_note = self.print_omitted_reason.clone();

        div()
            .id("filter-query-bar")
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.close(cx);
                    cx.stop_propagation();
                }
            }))
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_1p5()
            .bg(cx.theme().secondary)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .flex_shrink_0()
                            .child("Where"),
                    )
                    .child(
                        div().flex_1().min_w(px(240.)).child(
                            Input::new(&self.input)
                                .small()
                                .aria_label("Filter expression"),
                        ),
                    )
                    .child(
                        Button::new("query-bar-apply")
                            .label("Apply")
                            .primary()
                            .small()
                            .disabled(!can_apply)
                            .on_click(cx.listener(|this, _, _, cx| this.apply_now(cx))),
                    )
                    .child(
                        Button::new("query-bar-clear")
                            .label("Clear")
                            .ghost()
                            .small()
                            .disabled(!can_clear)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.clear_now(window, cx);
                            })),
                    )
                    .child(
                        Checkbox::new("query-bar-match-case")
                            .label("Match case")
                            .small()
                            .checked(self.match_case)
                            .on_change(cx.listener(|this, checked, window, cx| {
                                this.match_case = *checked;
                                this.draft_dirty = true;
                                this.draft_generation = this.draft_generation.wrapping_add(1);
                                this.debounce_task = None;
                                if let Some(Ok(query)) = this.revalidate_current(cx)
                                    && this.live_mode
                                {
                                    this.schedule_live_apply(Some(query), window, cx);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("query-bar-live")
                            .label("Live")
                            .small()
                            .checked(self.live_mode)
                            .on_change(cx.listener(|this, checked, window, cx| {
                                this.set_live_mode(*checked, window, cx);
                            })),
                    )
                    .child(
                        Button::new("query-bar-rules")
                            .label("Rules…")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(QueryBarEvent::OpenRules);
                            })),
                    )
                    .child(
                        Button::new("query-bar-switch-to-find")
                            .label("Find…")
                            .ghost()
                            .small()
                            .accessibility_label("Switch to Find bar")
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(QueryBarEvent::SwitchToFind);
                            })),
                    )
                    .when(info.filter_running, |row| {
                        row.child(
                            Button::new("query-bar-cancel")
                                .label("Cancel")
                                .small()
                                .danger()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let win = this.window_id;
                                    this.controller.update(cx, |c, cx| {
                                        c.cancel_filter_or_export_active(win, cx);
                                    });
                                })),
                        )
                    })
                    .when_some(info.filter_status.clone(), |row, status| {
                        row.child(
                            div()
                                .id("query-bar-status")
                                .role(Role::Status)
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .flex_shrink_0()
                                .child(status),
                        )
                    })
                    .child(
                        Button::new("close-query-bar")
                            .label("×")
                            .ghost()
                            .small()
                            .accessibility_label("Close Query bar")
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    ),
            )
            .when_some(diagnostic, |bar, message| {
                bar.child(
                    div()
                        .id("query-bar-error")
                        .role(Role::Alert)
                        .text_size(px(12.))
                        .text_color(cx.theme().danger)
                        .child(message),
                )
            })
            .when_some(omitted_note, |bar, note| {
                bar.child(
                    div()
                        .id("query-bar-omitted")
                        .role(Role::Status)
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "Active filter expression omitted ({note}); edit via Rules… or enter a new expression."
                        )),
                )
            })
            .into_any_element()
    }
}
