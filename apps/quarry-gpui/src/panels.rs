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
    CaseSensitivity, FilterOperator, FilterPredicate, FilterQuery, HeaderMode, OpenOptions,
};

use crate::document::DocumentInfo;

pub enum PanelEvent {
    Columns(Vec<usize>),
    AutoFit,
    Filter(FilterQuery),
    ClearFilter,
    Reopen(OpenOptions),
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

const OPERATORS: [(FilterOperator, &str); 8] = [
    (FilterOperator::Contains, "Contains"),
    (FilterOperator::Equals, "Equals"),
    (FilterOperator::NotEquals, "Does not equal"),
    (FilterOperator::GreaterThan, "Greater than"),
    (FilterOperator::GreaterThanOrEqual, "At least"),
    (FilterOperator::LessThan, "Less than"),
    (FilterOperator::LessThanOrEqual, "At most"),
    (FilterOperator::Between, "Between (inclusive)"),
];
const DELIMITERS: [&str; 5] = ["Auto", "Comma", "Tab", "Pipe", "Semicolon"];
const HEADERS: [&str; 3] = ["Auto", "First row is header", "No header"];

struct Rule {
    id: usize,
    column: Entity<ChoiceState>,
    operator: Entity<ChoiceState>,
    value: Entity<TextareaState>,
    upper: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Columns,
    Filters,
    Format,
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
    applied_format: (usize, usize),
    pending_format: Option<(usize, usize)>,
    details: bool,
    detected: String,
    busy: bool,
    dirty: bool,
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

impl Panels {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search columns"));
        let delimiter = select_state(DELIMITERS.map(str::to_owned), false, window, cx);
        let header = select_state(HEADERS.map(str::to_owned), false, window, cx);
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
            applied_format: (0, 0),
            pending_format: None,
            details: false,
            detected: String::new(),
            busy: false,
            dirty: false,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_document(
        &mut self,
        info: &DocumentInfo,
        shown: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dirty = info.dirty;
        self.detected = format!(
            "Detected: {} · {}",
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
            }
        );
        let added_columns = self.path.as_ref() == Some(&info.path)
            && info.headers.len() > self.headers.len()
            && info.headers.starts_with(&self.headers);
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
        } else if self.path.as_ref() != Some(&info.path) || self.headers != info.headers {
            self.close(window, cx);
            self.path = Some(info.path.clone());
            self.headers = info.headers.clone();
            let mut is_shown = vec![false; self.headers.len()];
            for &source in &shown {
                if let Some(flag) = is_shown.get_mut(source) {
                    *flag = true;
                }
            }
            self.order = shown.clone();
            self.order
                .extend((0..self.headers.len()).filter(|&source| !is_shown[source]));
            self.shown = is_shown;
            self.search
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.refresh_matches(cx);
            self.rules.clear();
            self.match_case = false;
            self.add_rule(window, cx);
        }
        if !self
            .order
            .iter()
            .copied()
            .filter(|source| self.shown[*source])
            .eq(shown.iter().copied())
        {
            // A successful reopen can reset the grid while keeping the same headers.
            let mut is_shown = vec![false; self.headers.len()];
            for &source in &shown {
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
            self.order = shown.clone();
            self.order.extend(hidden);
            self.shown = is_shown;
            self.refresh_matches(cx);
        }
        cx.notify();
    }

    pub fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        if self.busy != busy {
            self.busy = busy;
            cx.notify();
        }
    }

    pub fn finish_reopen(&mut self, success: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(format) = self.pending_format.take() {
            if success {
                self.applied_format = format;
                cx.notify();
            }
        } else if success {
            // A successful picker or startup Open uses OpenOptions::default() (Auto).
            self.applied_format = (0, 0);
            self.restore_format(window, cx);
            cx.notify();
        }
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
    }

    fn refresh_matches(&mut self, cx: &App) {
        let query = self.search.read(cx).value().to_lowercase();
        self.matches = self
            .order
            .iter()
            .copied()
            .filter(|source| {
                self.headers[*source].to_lowercase().contains(&query)
                    || (source + 1).to_string() == query
            })
            .collect();
    }

    fn emit_columns(&mut self, cx: &mut Context<Self>) {
        cx.emit(PanelEvent::Columns(
            self.order
                .iter()
                .copied()
                .filter(|source| self.shown[*source])
                .collect(),
        ));
        self.refresh_matches(cx);
        cx.notify();
    }

    fn add_rule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let column = select_state(
            self.headers
                .iter()
                .enumerate()
                .map(|(source, name)| format!("{}: {}", source + 1, name)),
            true,
            window,
            cx,
        );
        let operator = select_state(
            OPERATORS.map(|(_, label)| label.to_owned()),
            false,
            window,
            cx,
        );
        let value = cx.new(|cx| TextareaState::new(window, cx).rows(1).placeholder("Value"));
        let upper = cx.new(|cx| InputState::new(window, cx).placeholder("Upper bound"));
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
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel = cx.entity();
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
                    window.blur(cx);
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
                })
            })
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
        let title = format!("{}: {}", source + 1, self.headers[source]);
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
                    .checked(self.shown[source])
                    .small()
                    .flex_shrink_0()
                    .accessibility_label(format!("Show {title}"))
                    .disabled(
                        self.shown[source]
                            && self.shown.iter().filter(|shown| **shown).count() == 1,
                    )
                    .on_change(cx.listener(move |this, checked, _, cx| {
                        this.shown[source] = *checked;
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
                        let between = *rule.operator.read(cx).selected_value().unwrap_or(&0) == 7;
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
                                div().w(px(188.)).h(px(30.)).flex_shrink_0().min_w_0().child(Select::new(&rule.column)
                                    .id(("filter-column", id))
                                    .small()
                                    .h(px(30.))
                                    .w_full()
                                    .menu_width(px(320.))
                                    .menu_max_h(px(300.))
                                    .accessibility_label(format!("Rule {} column", position + 1))
                                    .search_placeholder("Search all columns")),
                            )
                            .child(
                                div().w(px(150.)).h(px(30.)).flex_shrink_0().min_w_0().child(Select::new(&rule.operator)
                                    .id(("filter-condition", id))
                                    .small()
                                    .h(px(30.))
                                    .w_full()
                                    .menu_width(px(210.))
                                    .accessibility_label(format!(
                                        "Rule {} condition",
                                        position + 1
                                    ))),
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
                                        this.child(Input::new(&rule.upper).small().w_full().min_w_0().aria_label(
                                            format!("Rule {} upper bound", position + 1),
                                        ))
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
            .child(div().flex().justify_start().child(Button::new("filter-details").label(if self.details { "▾ Details" } else { "▸ Details" }).ghost().small()
                .on_click(cx.listener(|this, _, _, cx| { this.details = !this.details; cx.notify(); }))))
            .when(self.details, |this| this.child(div().text_size(px(12.)).text_color(cx.theme().muted_foreground)
                .child("Rows must match every filtered column. Inclusion rules in one column are alternatives. Does not equal rules exclude matches. Text values are literal, including line breaks.")))
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
            .child(
                div()
                    .flex()
                    .justify_between()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
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
        );
        div()
            .flex()
            .flex_col()
            .w(px(292.))
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
            .when(self.dirty, |this| {
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("Undo all edits, or save a copy and open it, before changing the format."),
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
                            .disabled(self.busy || self.dirty || selected == self.applied_format)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.pending_format = Some(selected);
                                cx.emit(PanelEvent::Reopen(format_options(selected)));
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
        assert_eq!(OPERATORS.len(), 8);
        assert_eq!(OPERATORS[7].0, FilterOperator::Between);
        assert_eq!(format_options((2, 1)).delimiter, Some(b'\t'));
        assert_eq!(format_options((0, 2)).header_mode, HeaderMode::NoHeader);
    }
}
