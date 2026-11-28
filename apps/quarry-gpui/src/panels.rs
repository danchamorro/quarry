//! Toolbar popovers (Format, Columns, Filters) backed by `DocumentSnapshot` from `WorkbenchController`.

use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme, Disableable, IndexPath, Sizable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    popover::Popover,
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
};
use gpui_kit::{prelude::*, *};
use quarry_core::{
    CaseSensitivity, Encoding, FilterOperator, FilterPredicate, FilterQuery, HeaderMode,
    OpenOptions,
};

use crate::menus::{encoding_from_label, encoding_index, encoding_label};
use crate::workbench::{BookmarkListEntry, DocumentSnapshot};

pub enum PanelEvent {
    Columns(Vec<usize>),
    AutoFit,
    Filter(FilterQuery),
    ClearFilter,
    ExportFiltered,
    CancelFilterOrExport,
    Reopen(OpenOptions, Option<Encoding>),
    ToggleBookmarkCurrent,
    JumpToBookmark(u64),
    RemoveBookmark(u64),
    NextBookmark,
    PreviousBookmark,
    ClearBookmarks,
}

#[derive(Clone)]
struct Choice {
    source: usize,
    title: SharedString,
}

impl SelectItem for Choice {
    type Value = usize;

    fn title(&self) -> SharedString {
        self.title.clone()
    }
    fn value(&self) -> &usize {
        &self.source
    }
}

type ChoiceState = SelectState<SearchableVec<Choice>>;

const OPERATORS: [(FilterOperator, &str); 9] = [
    (FilterOperator::Contains, "Contains"),
    (FilterOperator::Equals, "Equals"),
    (FilterOperator::NotEquals, "Does not equal"),
    (FilterOperator::GreaterThan, "Greater than"),
    (FilterOperator::GreaterThanOrEqual, "At least"),
    (FilterOperator::LessThan, "Less than"),
    (FilterOperator::LessThanOrEqual, "At most"),
    (FilterOperator::Between, "Between (inclusive)"),
    (FilterOperator::Regex, "Matches regex"),
];
const DELIMITERS: [&str; 5] = ["Auto", "Comma", "Tab", "Pipe", "Semicolon"];
const HEADERS: [&str; 3] = ["Auto", "First row is header", "No header"];
const ENCODINGS: [&str; 6] = [
    "Auto",
    "UTF-8",
    "UTF-16 LE",
    "UTF-16 BE",
    "Windows-1252",
    "ISO-8859-1",
];

struct Rule {
    id: usize,
    column: Entity<ChoiceState>,
    operator: Entity<ChoiceState>,
    value: Entity<TextareaState>,
    upper: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Columns,
    Filters,
    Format,
    Bookmarks,
}

pub struct Panels {
    path: Option<PathBuf>,
    headers: Vec<String>,
    order: Vec<usize>,
    shown: Vec<bool>,
    matches: Vec<usize>,
    search: Entity<InputState>,
    rules: Vec<Rule>,
    next_rule: usize,
    match_case: bool,
    active: Option<Tool>,
    delimiter: Entity<ChoiceState>,
    header: Entity<ChoiceState>,
    encoding: Entity<ChoiceState>,
    applied_format: (usize, usize, usize),
    details: bool,
    detected: String,
    busy: bool,
    dirty: bool,
    live_watching: bool,
    source_changed: bool,
    filter_active: bool,
    filter_running: bool,
    last_synced_filter: Option<(u64, u64)>,
    latest_filter_key: Option<(u64, u64)>,
    latest_filter_query: Option<FilterQuery>,
    non_utf8_query_notice: bool,
    can_export_filtered: bool,
    export_running: bool,
    filter_status: Option<String>,
    export_status: Option<String>,
    bookmark_count: usize,
    bookmarks: Vec<BookmarkListEntry>,
    speculative_section: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for Panels {}

fn choices(labels: impl IntoIterator<Item = String>) -> SearchableVec<Choice> {
    SearchableVec::new(
        labels
            .into_iter()
            .enumerate()
            .map(|(source, title)| Choice {
                source,
                title: title.into(),
            })
            .collect::<Vec<_>>(),
    )
}

fn select_state(
    labels: impl IntoIterator<Item = String>,
    searchable: bool,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ChoiceState> {
    cx.new(|cx| {
        SelectState::new(choices(labels), Some(IndexPath::new(0)), window, cx)
            .searchable(searchable)
    })
}

fn delimiter_index(delimiter: Option<u8>) -> usize {
    match delimiter {
        None => 0,
        Some(b',') => 1,
        Some(b'\t') => 2,
        Some(b'|') => 3,
        Some(b';') => 4,
        Some(_) => 0,
    }
}

fn header_index(mode: HeaderMode) -> usize {
    match mode {
        HeaderMode::Auto => 0,
        HeaderMode::FirstRow => 1,
        HeaderMode::NoHeader => 2,
    }
}

impl Panels {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search columns"));
        let delimiter = select_state(DELIMITERS.map(str::to_owned), false, window, cx);
        let header = select_state(HEADERS.map(str::to_owned), false, window, cx);
        let encoding = select_state(ENCODINGS.map(str::to_owned), false, window, cx);
        let subscriptions = vec![
            cx.subscribe_in(&search, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.refresh_matches(cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &delimiter,
                window,
                |_, _, _: &SelectEvent<SearchableVec<Choice>>, _, cx| cx.notify(),
            ),
            cx.subscribe_in(
                &header,
                window,
                |_, _, _: &SelectEvent<SearchableVec<Choice>>, _, cx| cx.notify(),
            ),
            cx.subscribe_in(
                &encoding,
                window,
                |_, _, _: &SelectEvent<SearchableVec<Choice>>, _, cx| cx.notify(),
            ),
        ];
        Self {
            path: None,
            headers: vec![],
            order: vec![],
            shown: vec![],
            matches: vec![],
            search,
            rules: vec![],
            next_rule: 0,
            match_case: false,
            active: None,
            delimiter,
            header,
            encoding,
            applied_format: (0, 0, 0),
            details: false,
            detected: String::new(),
            busy: false,
            dirty: false,
            live_watching: false,
            source_changed: false,
            filter_active: false,
            filter_running: false,
            last_synced_filter: None,
            latest_filter_key: None,
            latest_filter_query: None,
            non_utf8_query_notice: false,
            can_export_filtered: false,
            export_running: false,
            filter_status: None,
            export_status: None,
            bookmark_count: 0,
            bookmarks: Vec::new(),
            speculative_section: false,
            _subscriptions: subscriptions,
        }
    }

    pub fn open_tool(&mut self, tool: Tool, window: &mut Window, cx: &mut Context<Self>) {
        if self.path.is_none() {
            return;
        }
        if self.active == Some(tool) {
            self.close(window, cx);
        } else {
            if tool == Tool::Format {
                self.restore_format(window, cx);
            } else if tool == Tool::Filters && self.last_synced_filter != self.latest_filter_key {
                let query = self.latest_filter_query.clone();
                self.sync_rules_from_query(query.as_ref(), window, cx);
                self.last_synced_filter = self.latest_filter_key;
            }
            self.active = Some(tool);
            cx.notify();
        }
    }

    pub fn clear_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path.is_some() {
            self.close(window, cx);
            self.path = None;
            self.headers.clear();
            self.order.clear();
            self.shown.clear();
            self.matches.clear();
            self.rules.clear();
            self.last_synced_filter = None;
            self.latest_filter_key = None;
            self.latest_filter_query = None;
            self.non_utf8_query_notice = false;
            self.dirty = false;
            self.busy = false;
            self.live_watching = false;
            self.source_changed = false;
            self.bookmark_count = 0;
            self.bookmarks.clear();
            self.speculative_section = false;
            self.detected.clear();
            cx.notify();
        }
    }

    pub fn sync_document(
        &mut self,
        info: &DocumentSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dirty = info.dirty;
        self.busy = info.busy;
        self.live_watching = info.live_watch.active;
        self.source_changed = info.source_changed;
        self.filter_active = info.filter_active;
        self.filter_running = info.filter_running;
        self.can_export_filtered = info.can_export_filtered;
        self.export_running = info.export_running;
        self.filter_status = info.filter_status.clone();
        self.export_status = info.export_status.clone();
        self.bookmark_count = info.bookmark_count;
        self.bookmarks = info.bookmarks.clone();
        self.speculative_section = info
            .section
            .as_ref()
            .is_some_and(|section| section.bounds.trust == quarry_core::BoundaryTrust::Speculative);
        self.detected = format!(
            "Detected: {} · {} · {}",
            match info.delimiter {
                b',' => "Comma",
                b'\t' => "Tab",
                b'|' => "Pipe",
                b';' => "Semicolon",
                _ => "Custom delimiter",
            },
            if info.has_header {
                "Header row"
            } else {
                "No header"
            },
            encoding_label(info.encoding),
        );
        let applied = (
            delimiter_index(info.applied_delimiter),
            header_index(info.applied_header_mode),
            encoding_index(info.applied_encoding),
        );
        if self.applied_format != applied {
            self.applied_format = applied;
            if self.active != Some(Tool::Format) {
                self.restore_format(window, cx);
            }
        }
        let added_columns = self.path.as_ref() == Some(&info.path)
            && info.headers.len() > self.headers.len()
            && info.headers.starts_with(&self.headers);
        let renamed_headers = self.path.as_ref() == Some(&info.path)
            && info.headers.len() == self.headers.len()
            && info.headers != self.headers;
        if added_columns {
            let previous_count = self.headers.len();
            self.headers = info.headers.clone();
            self.order.extend(previous_count..self.headers.len());
            self.shown.resize(self.headers.len(), true);
            for rule in &self.rules {
                let selected = *rule.column.read(cx).selected_value().unwrap_or(&0);
                rule.column.update(cx, |state, cx| {
                    state.set_items(
                        choices(
                            self.headers
                                .iter()
                                .enumerate()
                                .map(|(source, name)| format!("{}: {}", source + 1, name)),
                        ),
                        window,
                        cx,
                    );
                    state.set_selected_value(&selected, window, cx);
                });
            }
            self.refresh_matches(cx);
        } else if renamed_headers {
            self.headers = info.headers.clone();
            for rule in &self.rules {
                let selected = *rule.column.read(cx).selected_value().unwrap_or(&0);
                rule.column.update(cx, |state, cx| {
                    state.set_items(
                        choices(
                            self.headers
                                .iter()
                                .enumerate()
                                .map(|(source, name)| format!("{}: {}", source + 1, name)),
                        ),
                        window,
                        cx,
                    );
                    state.set_selected_value(&selected, window, cx);
                });
            }
            self.refresh_matches(cx);
        } else if self.path.as_ref() != Some(&info.path) || self.headers != info.headers {
            self.close(window, cx);
            self.path = Some(info.path.clone());
            self.headers = info.headers.clone();
            let mut is_shown = vec![false; self.headers.len()];
            for &source in &info.shown_columns {
                if let Some(flag) = is_shown.get_mut(source) {
                    *flag = true;
                }
            }
            self.order = info.shown_columns.clone();
            self.order
                .extend((0..self.headers.len()).filter(|&source| !is_shown[source]));
            self.shown = is_shown;
            self.search
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.refresh_matches(cx);
            self.rules.clear();
            self.match_case = false;
            self.non_utf8_query_notice = false;
            self.last_synced_filter = None;
            self.add_rule(window, cx);
        }
        if !self
            .order
            .iter()
            .copied()
            .filter(|source| self.shown.get(*source).copied().unwrap_or(false))
            .eq(info.shown_columns.iter().copied())
        {
            let mut is_shown = vec![false; self.headers.len()];
            for &source in &info.shown_columns {
                if let Some(flag) = is_shown.get_mut(source) {
                    *flag = true;
                }
            }
            let hidden = self
                .order
                .iter()
                .copied()
                .filter(|&source| !is_shown.get(source).copied().unwrap_or(false))
                .collect::<Vec<_>>();
            self.order = info.shown_columns.clone();
            self.order.extend(hidden);
            self.shown = is_shown;
            self.refresh_matches(cx);
        }
        let filter_sync_key = (info.instance_id, info.filter_query_generation);
        let query_changed = self.latest_filter_query != info.filter_query;
        self.latest_filter_key = Some(filter_sync_key);
        self.latest_filter_query = info.filter_query.clone();
        if self.active != Some(Tool::Filters)
            && (self.last_synced_filter != Some(filter_sync_key) || query_changed)
        {
            self.sync_rules_from_query(info.filter_query.as_ref(), window, cx);
            self.last_synced_filter = Some(filter_sync_key);
        }
        cx.notify();
    }

    fn sync_rules_from_query(
        &mut self,
        query: Option<&FilterQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(query) = query else {
            self.non_utf8_query_notice = false;
            self.rules.clear();
            self.match_case = false;
            self.add_rule(window, cx);
            return;
        };
        // Non-UTF-8 byte predicates cannot be edited losslessly in UTF-8 rule textboxes:
        // clear stale rules and display an explicit banner directing the user to the Query Bar.
        if query.predicates.iter().any(|p| {
            std::str::from_utf8(&p.value).is_err()
                || p.upper_bound
                    .as_deref()
                    .is_some_and(|u| std::str::from_utf8(u).is_err())
        }) {
            self.non_utf8_query_notice = true;
            self.rules.clear();
            self.match_case = query.case_sensitivity == CaseSensitivity::Sensitive;
            self.add_rule(window, cx);
            return;
        }
        self.non_utf8_query_notice = false;
        self.rules.clear();
        self.match_case = query.case_sensitivity == CaseSensitivity::Sensitive;
        for predicate in &query.predicates {
            let op_idx = OPERATORS
                .iter()
                .position(|(op, _)| *op == predicate.operator)
                .unwrap_or(0);
            let value_str = std::str::from_utf8(&predicate.value)
                .unwrap_or("")
                .to_owned();
            let upper_str = predicate
                .upper_bound
                .as_deref()
                .and_then(|u| std::str::from_utf8(u).ok())
                .unwrap_or("")
                .to_owned();
            self.add_rule_with_values(predicate.column, op_idx, value_str, upper_str, window, cx);
        }
        if self.rules.is_empty() {
            self.add_rule(window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn synced_query_for_test(&self, cx: &App) -> FilterQuery {
        self.query(cx)
    }

    #[cfg(test)]
    pub(crate) fn has_non_utf8_query_notice_for_test(&self) -> bool {
        self.non_utf8_query_notice
    }

    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Moving focus first lets Kit dismiss a nested Select before its owner disappears.
        window.blur(cx);
        self.active = None;
        cx.notify();
    }

    fn restore_format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.delimiter.update(cx, |state, cx| {
            state.set_selected_value(&self.applied_format.0, window, cx)
        });
        self.header.update(cx, |state, cx| {
            state.set_selected_value(&self.applied_format.1, window, cx)
        });
        self.encoding.update(cx, |state, cx| {
            state.set_selected_value(&self.applied_format.2, window, cx)
        });
    }

    fn refresh_matches(&mut self, cx: &App) {
        let query = self.search.read(cx).value().to_lowercase();
        self.matches = self
            .order
            .iter()
            .copied()
            .filter(|source| {
                self.headers
                    .get(*source)
                    .is_some_and(|h| h.to_lowercase().contains(&query))
                    || (source + 1).to_string() == query
            })
            .collect();
    }

    fn emit_columns(&mut self, cx: &mut Context<Self>) {
        cx.emit(PanelEvent::Columns(
            self.order
                .iter()
                .copied()
                .filter(|source| self.shown.get(*source).copied().unwrap_or(false))
                .collect(),
        ));
        self.refresh_matches(cx);
        cx.notify();
    }

    fn add_rule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_rule_with_values(0, 0, String::new(), String::new(), window, cx);
    }

    fn add_rule_with_values(
        &mut self,
        selected_column: usize,
        selected_operator: usize,
        initial_value: String,
        initial_upper: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let column = select_state(
            self.headers
                .iter()
                .enumerate()
                .map(|(source, name)| format!("{}: {}", source + 1, name)),
            true,
            window,
            cx,
        );
        if selected_column > 0 {
            column.update(cx, |state, cx| {
                state.set_selected_value(&selected_column, window, cx)
            });
        }
        let operator = select_state(
            OPERATORS.map(|(_, label)| label.to_owned()),
            false,
            window,
            cx,
        );
        if selected_operator > 0 {
            operator.update(cx, |state, cx| {
                state.set_selected_value(&selected_operator, window, cx)
            });
        }
        let value = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).rows(1).placeholder("Value");
            if !initial_value.is_empty() {
                state.set_value(initial_value, window, cx);
            }
            state
        });
        let upper = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Upper bound");
            if !initial_upper.is_empty() {
                state.set_value(initial_upper, window, cx);
            }
            state
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &column,
                window,
                |_, _, _: &SelectEvent<SearchableVec<Choice>>, _, cx| cx.notify(),
            ),
            cx.subscribe_in(
                &operator,
                window,
                |_, _, _: &SelectEvent<SearchableVec<Choice>>, _, cx| cx.notify(),
            ),
            cx.subscribe_in(&value, window, |_, _, _: &InputEvent, _, cx| cx.notify()),
            cx.subscribe_in(&upper, window, |_, _, _: &InputEvent, _, cx| cx.notify()),
        ];
        self.rules.push(Rule {
            id: self.next_rule,
            column,
            operator,
            value,
            upper,
            _subscriptions: subscriptions,
        });
        self.next_rule += 1;
        cx.notify();
    }

    fn query(&self, cx: &App) -> FilterQuery {
        FilterQuery {
            predicates: self
                .rules
                .iter()
                .map(|rule| {
                    let operator =
                        OPERATORS[*rule.operator.read(cx).selected_value().unwrap_or(&0)].0;
                    FilterPredicate {
                        column: *rule.column.read(cx).selected_value().unwrap_or(&0),
                        operator,
                        value: rule.value.read(cx).value().as_bytes().to_vec(),
                        upper_bound: (operator == FilterOperator::Between)
                            .then(|| rule.upper.read(cx).value().as_bytes().to_vec()),
                    }
                })
                .collect(),
            case_sensitivity: if self.match_case {
                CaseSensitivity::Sensitive
            } else {
                CaseSensitivity::Insensitive
            },
        }
    }

    fn popover(
        &self,
        tool: Tool,
        id: &'static str,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel = cx.entity();
        div()
            // Switch directly to another toolbar panel in capture phase before a
            // nested Select's bubble `on_mouse_down_out` calls `stop_propagation`.
            .capture_any_mouse_down(
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if event.button == MouseButton::Left
                        && this.path.is_some()
                        && this.active.is_some()
                        && this.active != Some(tool)
                    {
                        if tool == Tool::Format {
                            this.restore_format(window, cx);
                        }
                        this.active = Some(tool);
                        cx.notify();
                    }
                }),
            )
            .child(
                Popover::new(id)
                    .anchor(Anchor::TopRight)
                    .open(self.active == Some(tool))
                    .trigger(
                        Button::new((id, 0usize))
                            .label(label)
                            .ghost()
                            .small()
                            .disabled(self.path.is_none()),
                    )
                    .on_open_change(cx.listener(move |this, open, window, cx| {
                        if *open {
                            if tool == Tool::Format {
                                this.restore_format(window, cx);
                            }
                            this.active = Some(tool);
                        } else if this.active == Some(tool) {
                            this.close(window, cx);
                        }
                        cx.notify();
                    }))
                    .content(move |_, window, cx| {
                        panel.update(cx, |this, cx| match tool {
                            Tool::Columns => this.columns_content(window, cx),
                            Tool::Filters => this.filters_content(window, cx),
                            Tool::Format => this.format_content(window, cx),
                            Tool::Bookmarks => this.bookmarks_content(window, cx),
                        })
                    }),
            )
            .into_any_element()
    }

    pub fn render_columns(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.popover(Tool::Columns, "columns-popover", "Columns", cx)
    }

    pub fn render_filters(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.popover(Tool::Filters, "filters-popover", "Filters", cx)
    }

    pub fn render_format(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.popover(Tool::Format, "format-popover", "Format", cx)
    }

    pub fn render_bookmarks(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let label = if self.bookmark_count > 0 {
            format!("Bookmarks ({})", self.bookmark_count)
        } else {
            "Bookmarks".to_owned()
        };
        self.popover(Tool::Bookmarks, "bookmarks-popover", label, cx)
    }

    fn bookmarks_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let panel = cx.entity();
        let height = (f32::from(window.viewport_size().height) - 270.).clamp(84., 300.);
        let count = self.bookmark_count;
        let has_bookmarks = count > 0;
        let disabled = self.source_changed;
        let mut content = div()
            .flex()
            .flex_col()
            .w(px(360.))
            .gap_2()
            .text_size(px(13.))
            .child(self.heading("Bookmarks", cx))
            .child(
                div().flex().items_center().justify_between().gap_2().child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(if has_bookmarks {
                            format!(
                                "{count} bookmark{} · session-only (max {})",
                                if count == 1 { "" } else { "s" },
                                quarry_workbench::group_digits(
                                    quarry_workbench::MAX_ROW_BOOKMARKS as u64
                                )
                            )
                        } else {
                            "No bookmarks yet · right-click # or press Cmd/Ctrl+B".to_owned()
                        }),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new("bookmark-toggle-current")
                            .label("Toggle Row")
                            .small()
                            .disabled(disabled)
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(PanelEvent::ToggleBookmarkCurrent);
                            })),
                    )
                    .child(
                        Button::new("bookmark-prev")
                            .label("Prev")
                            .ghost()
                            .small()
                            .disabled(disabled || !has_bookmarks)
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(PanelEvent::PreviousBookmark);
                            })),
                    )
                    .child(
                        Button::new("bookmark-next")
                            .label("Next")
                            .ghost()
                            .small()
                            .disabled(disabled || !has_bookmarks)
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(PanelEvent::NextBookmark);
                            })),
                    )
                    .child(
                        Button::new("bookmark-clear-all")
                            .label("Clear All")
                            .ghost()
                            .small()
                            .disabled(disabled || !has_bookmarks)
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(PanelEvent::ClearBookmarks);
                            })),
                    ),
            );
        if self.speculative_section {
            content = content.child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "Speculative section: navigation works, but Split at Bookmarks requires Data > Verify Section Boundary.",
                    ),
            );
        }
        if has_bookmarks {
            content = content.child(
                uniform_list(
                    "bookmark-rows",
                    self.bookmarks.len(),
                    move |range, _, cx| {
                        let start = range.start;
                        panel.update(cx, |this, cx| {
                            range
                                .filter_map(|index| this.bookmarks.get(index).cloned())
                                .enumerate()
                                .map(|(offset_ix, entry)| {
                                    this.bookmark_row(start + offset_ix, entry, cx)
                                })
                                .collect::<Vec<_>>()
                        })
                    },
                )
                .h(px(height.min(self.bookmarks.len().max(1) as f32 * 30.)))
                .w_full(),
            );
        }
        content
            .child(
                div()
                    .flex()
                    .justify_end()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("bookmarks-done")
                            .label("Done")
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                    ),
            )
            .into_any_element()
    }

    fn bookmark_row(
        &self,
        index: usize,
        entry: BookmarkListEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let local_row = entry.local_row;
        let offset_text = entry.record_start_offset.map_or_else(
            || "offset indexing…".to_owned(),
            |offset| format!("@ {} B", quarry_workbench::group_digits(offset)),
        );
        div()
            .flex()
            .h(px(30.))
            .w_full()
            .min_w_0()
            .items_center()
            .justify_between()
            .gap_2()
            .px_1()
            .rounded(px(4.))
            .when(index % 2 == 1, |this| {
                this.bg(cx.theme().muted.opacity(0.45))
            })
            .child(
                Button::new(("bookmark-jump", index))
                    .label(format!("{} · {}", entry.label, offset_text))
                    .ghost()
                    .small()
                    .flex_1()
                    .min_w_0()
                    .disabled(self.source_changed)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.emit(PanelEvent::JumpToBookmark(local_row));
                        this.close(window, cx);
                    })),
            )
            .child(
                Button::new(("bookmark-remove", index))
                    .label("×")
                    .ghost()
                    .small()
                    .accessibility_label(format!("Remove bookmark for {}", entry.label))
                    .disabled(self.source_changed)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(PanelEvent::RemoveBookmark(local_row));
                    })),
            )
            .into_any_element()
    }

    fn heading(&self, title: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(
                Button::new((title, 1usize))
                    .label("×")
                    .ghost()
                    .small()
                    .accessibility_label(format!("Close {title}"))
                    .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
            )
    }

    fn columns_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let panel = cx.entity();
        let height = (f32::from(window.viewport_size().height) - 250.).clamp(84., 336.);
        let shown_count = self.shown.iter().filter(|shown| **shown).count();
        div()
            .flex()
            .flex_col()
            .w(px(336.))
            .gap_2()
            .text_size(px(13.))
            .child(self.heading("Columns", cx))
            .child(
                Input::new(&self.search)
                    .small()
                    .aria_label("Search columns")
                    .cleanable(true),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_color(cx.theme().muted_foreground).child(format!(
                        "{} shown of {}",
                        shown_count,
                        self.headers.len()
                    )))
                    .child(
                        Button::new("reset-columns")
                            .label("Reset")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.order = (0..this.headers.len()).collect();
                                this.shown.fill(true);
                                this.emit_columns(cx);
                            })),
                    ),
            )
            .child(
                uniform_list("column-rows", self.matches.len(), move |range, _, cx| {
                    panel.update(cx, |this, cx| {
                        range
                            .filter_map(|index| this.matches.get(index).copied())
                            .map(|source| this.column_row(source, cx))
                            .collect::<Vec<_>>()
                    })
                })
                .h(px(height.min(self.matches.len().max(1) as f32 * 30.)))
                .w_full(),
            )
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .text_size(px(12.))
                    .child("Uncheck to hide · Use arrows to reorder"),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("auto-fit-columns")
                            .label("Auto-fit columns")
                            .small()
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::AutoFit))),
                    )
                    .child(
                        Button::new("columns-done")
                            .label("Done")
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                    ),
            )
            .into_any_element()
    }

    fn column_row(&self, source: usize, cx: &mut Context<Self>) -> AnyElement {
        let position = self
            .order
            .iter()
            .position(|candidate| *candidate == source)
            .unwrap_or(0);
        let title = format!(
            "{}: {}",
            source + 1,
            self.headers.get(source).map(String::as_str).unwrap_or("")
        );
        let is_shown = self.shown.get(source).copied().unwrap_or(true);
        div()
            .flex()
            .h(px(30.))
            .w_full()
            .min_w_0()
            .items_center()
            .gap_2()
            .px_1()
            .rounded(px(4.))
            .when(position % 2 == 1, |this| {
                this.bg(cx.theme().muted.opacity(0.45))
            })
            .child(
                Checkbox::new(("show-column", source))
                    .checked(is_shown)
                    .small()
                    .flex_shrink_0()
                    .accessibility_label(format!("Show {title}"))
                    .disabled(is_shown && self.shown.iter().filter(|shown| **shown).count() == 1)
                    .on_change(cx.listener(move |this, checked, _, cx| {
                        if let Some(slot) = this.shown.get_mut(source) {
                            *slot = *checked;
                        }
                        this.emit_columns(cx);
                    })),
            )
            .child(div().flex_1().min_w_0().truncate().child(title.clone()))
            .child(
                Button::new(("column-up", source))
                    .label("↑")
                    .ghost()
                    .small()
                    .w(px(24.))
                    .flex_shrink_0()
                    .accessibility_label(format!("Move {title} left"))
                    .disabled(position == 0)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.order.swap(position, position - 1);
                        this.emit_columns(cx);
                    })),
            )
            .child(
                Button::new(("column-down", source))
                    .label("↓")
                    .ghost()
                    .small()
                    .w(px(24.))
                    .flex_shrink_0()
                    .accessibility_label(format!("Move {title} right"))
                    .disabled(position + 1 == self.order.len())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.order.swap(position, position + 1);
                        this.emit_columns(cx);
                    })),
            )
            .into_any_element()
    }

    fn filters_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let query = self.query(cx);
        let validation = query.validate();
        let max_height = (f32::from(window.viewport_size().height) - 250.).clamp(80., 320.);
        div()
            .flex()
            .flex_col()
            .w(px(576.))
            .gap_2()
            .text_size(px(13.))
            .child(self.heading("Filters", cx))
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .text_size(px(12.))
                    .child("Choose which rows stay in view."),
            )
            .when(self.non_utf8_query_notice, |this| {
                this.child(
                    div()
                        .id("filter-non-utf8-notice")
                        .role(Role::Status)
                        .text_size(px(12.))
                        .text_color(cx.theme().warning_foreground)
                        .child("Active filter contains raw non-UTF-8 bytes; edit it in the Query Bar or enter new rules here to replace it."),
                )
            })
            .child(
                div()
                    .flex()
                    .gap_2()
                    .pl(px(24.))
                    .text_color(cx.theme().muted_foreground)
                    .text_size(px(12.))
                    .child(div().w(px(188.)).flex_shrink_0().child("Column"))
                    .child(div().w(px(150.)).flex_shrink_0().child("Condition"))
                    .child(div().flex_1().min_w_0().child("Value")),
            )
            .child(
                div()
                    .id("filter-rules")
                    .flex()
                    .flex_col()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .max_h(px(max_height))
                    .overflow_y_scroll()
                    .children(self.rules.iter().enumerate().map(|(position, rule)| {
                        let id = rule.id;
                        let between = OPERATORS
                            [*rule.operator.read(cx).selected_value().unwrap_or(&0)]
                            .0
                            == FilterOperator::Between;
                        div()
                            .flex()
                            .w_full()
                            .min_w_0()
                            .items_start()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(16.))
                                    .flex_shrink_0()
                                    .pt_1()
                                    .text_color(cx.theme().muted_foreground)
                                    .child((position + 1).to_string()),
                            )
                            .child(
                                div()
                                    .w(px(188.))
                                    .h(px(30.))
                                    .flex_shrink_0()
                                    .min_w_0()
                                    .child(
                                        Select::new(&rule.column)
                                            .id(("filter-column", id))
                                            .small()
                                            .h(px(30.))
                                            .w_full()
                                            .menu_width(px(320.))
                                            .menu_max_h(px(300.))
                                            .accessibility_label(format!(
                                                "Rule {} column",
                                                position + 1
                                            ))
                                            .search_placeholder("Search all columns"),
                                    ),
                            )
                            .child(
                                div()
                                    .w(px(150.))
                                    .h(px(30.))
                                    .flex_shrink_0()
                                    .min_w_0()
                                    .child(
                                        Select::new(&rule.operator)
                                            .id(("filter-condition", id))
                                            .small()
                                            .h(px(30.))
                                            .w_full()
                                            .menu_width(px(210.))
                                            .accessibility_label(format!(
                                                "Rule {} condition",
                                                position + 1
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .w(px(166.))
                                    .flex_shrink_0()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        Textarea::new(&rule.value)
                                            .w_full()
                                            .min_w_0()
                                            .h(px(30.))
                                            .aria_label(format!("Rule {} value", position + 1)),
                                    )
                                    .when(between, |this| {
                                        this.child(
                                            Input::new(&rule.upper)
                                                .small()
                                                .w_full()
                                                .min_w_0()
                                                .aria_label(format!(
                                                    "Rule {} upper bound",
                                                    position + 1
                                                )),
                                        )
                                    }),
                            )
                            .child(
                                Button::new(("remove-rule", id))
                                    .label("×")
                                    .ghost()
                                    .small()
                                    .w(px(24.))
                                    .flex_shrink_0()
                                    .disabled(self.rules.len() == 1)
                                    .accessibility_label(format!("Remove rule {}", position + 1))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.rules.retain(|rule| rule.id != id);
                                        cx.notify();
                                    })),
                            )
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        Button::new("add-filter-rule")
                            .label("+ Add rule")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| this.add_rule(window, cx))),
                    )
                    .child(
                        Checkbox::new("filter-match-case")
                            .label("Match case")
                            .small()
                            .checked(self.match_case)
                            .on_change(cx.listener(|this, checked, _, cx| {
                                this.match_case = *checked;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div().flex().justify_start().child(
                    Button::new("filter-details")
                        .label(if self.details {
                            "▾ Details"
                        } else {
                            "▸ Details"
                        })
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.details = !this.details;
                            cx.notify();
                        })),
                ),
            )
            .when(self.details, |this| {
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("Rows must match every filtered column. Inclusion rules in one column are alternatives. Does not equal rules exclude matches. Text values are literal, including line breaks."),
                )
            })
            .when(
                self.rules
                    .iter()
                    .any(|rule| !rule.value.read(cx).value().is_empty()),
                |this| {
                    this.when_some(validation.as_ref().err(), |this, error| {
                        this.child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(error.to_string()),
                        )
                    })
                },
            )
            .when_some(
                self.export_status
                    .clone()
                    .or_else(|| self.filter_status.clone()),
                |this, status| {
                    this.child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(status)
                            .when(self.filter_running || self.export_running, |row| {
                                row.child(
                                    Button::new("cancel-filter-export")
                                        .label("Cancel")
                                        .small()
                                        .ghost()
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(PanelEvent::CancelFilterOrExport);
                                        })),
                                )
                            }),
                    )
                },
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .gap_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("clear-filter")
                                    .label("Clear filter")
                                    .small()
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        cx.emit(PanelEvent::ClearFilter);
                                        this.close(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("export-filtered")
                                    .label("Export Filtered…")
                                    .small()
                                    .disabled(!self.can_export_filtered || self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        cx.emit(PanelEvent::ExportFiltered);
                                        this.close(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        Button::new("apply-filters")
                            .label("Apply filters")
                            .primary()
                            .small()
                            .disabled(self.busy || validation.is_err())
                            .on_click(cx.listener(|this, _, window, cx| {
                                let query = this.query(cx);
                                if query.validate().is_ok() {
                                    cx.emit(PanelEvent::Filter(query));
                                    this.close(window, cx);
                                }
                            })),
                    ),
            )
            .into_any_element()
    }

    fn format_content(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let selected = (
            *self.delimiter.read(cx).selected_value().unwrap_or(&0),
            *self.header.read(cx).selected_value().unwrap_or(&0),
            *self.encoding.read(cx).selected_value().unwrap_or(&0),
        );
        div()
            .flex()
            .flex_col()
            .w(px(304.))
            .gap_3()
            .text_size(px(13.))
            .child(self.heading("Format", cx))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(self.detected.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(68.)).flex_shrink_0().child("Delimiter"))
                    .child(
                        div()
                            .w(px(216.))
                            .h(px(30.))
                            .flex_shrink_0()
                            .min_w_0()
                            .child(
                                Select::new(&self.delimiter)
                                    .id("format-delimiter")
                                    .small()
                                    .h(px(30.))
                                    .w_full()
                                    .accessibility_label("Delimiter"),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(68.)).flex_shrink_0().child("Header"))
                    .child(
                        div()
                            .w(px(216.))
                            .h(px(30.))
                            .flex_shrink_0()
                            .min_w_0()
                            .child(
                                Select::new(&self.header)
                                    .id("format-header")
                                    .small()
                                    .h(px(30.))
                                    .w_full()
                                    .accessibility_label("Header row"),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(68.)).flex_shrink_0().child("Encoding"))
                    .child(
                        div()
                            .w(px(216.))
                            .h(px(30.))
                            .flex_shrink_0()
                            .min_w_0()
                            .child(
                                Select::new(&self.encoding)
                                    .id("format-encoding")
                                    .small()
                                    .h(px(30.))
                                    .w_full()
                                    .accessibility_label("Text encoding"),
                            ),
                    ),
            )
            .when(self.dirty, |this| {
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("Save or discard your changes before changing the format."),
                )
            })
            .when(self.live_watching, |this| {
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("Stop Live Watch before changing format or encoding."),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("format-cancel")
                            .label("Cancel")
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.restore_format(window, cx);
                                this.close(window, cx);
                            })),
                    )
                    .child(
                        Button::new("format-reopen")
                            .label("Reopen")
                            .primary()
                            .small()
                            .disabled(
                                self.busy
                                    || self.dirty
                                    || self.live_watching
                                    || selected == self.applied_format,
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.emit(PanelEvent::Reopen(
                                    format_options((selected.0, selected.1)),
                                    encoding_from_label(selected.2),
                                ));
                                this.close(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

fn format_options((delimiter, header): (usize, usize)) -> OpenOptions {
    OpenOptions {
        delimiter: [None, Some(b','), Some(b'\t'), Some(b'|'), Some(b';')][delimiter],
        header_mode: [HeaderMode::Auto, HeaderMode::FirstRow, HeaderMode::NoHeader][header],
        ..OpenOptions::default()
    }
}

impl Render for Panels {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .gap_1()
            .child(self.render_format(window, cx))
            .child(self.render_columns(window, cx))
            .child(self.render_filters(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{Choice, OPERATORS, SelectItem, format_options};
    use quarry_core::{FilterOperator, HeaderMode};

    #[test]
    fn selectors_keep_source_identity_and_all_engine_choices() {
        let first = Choice {
            source: 0,
            title: "Duplicate".into(),
        };
        let last = Choice {
            source: 663,
            title: "Duplicate".into(),
        };
        assert_ne!(first.value(), last.value());
        assert_eq!(OPERATORS.len(), 9);
        assert_eq!(OPERATORS[7].0, FilterOperator::Between);
        assert_eq!(OPERATORS[8].0, FilterOperator::Regex);
        assert_eq!(format_options((2, 1)).delimiter, Some(b'\t'));
        assert_eq!(format_options((0, 2)).header_mode, HeaderMode::NoHeader);
    }
}
