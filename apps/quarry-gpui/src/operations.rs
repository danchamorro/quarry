//! Modal dialogs for Phase 4B-2b data operations: Multi-column Sort (1–16 levels),
//! Find & Remove Duplicates, Cleanup Selected Columns, Insert Rows/Columns, Move Columns,
//! Split Column, and Combine Columns.

use std::collections::BTreeSet;

use gpui_kit::component::{
    ActiveTheme, Disableable, IndexPath, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    dialog::{Cancel as DialogCancel, Confirm as DialogConfirm},
    input::{Input, InputState},
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
};
use gpui_kit::{prelude::*, *};
use quarry_core::{
    CaseSensitivity, CleanupOperation, ColumnCleanup, DateTimeFormat, DuplicateTarget,
    SortDirection, SortMode, SortPlan, SortSpec,
};
use quarry_workbench::{DocumentId, StructuralDialog, WindowId, format_bytes};

use crate::workbench::WorkbenchController;

#[derive(Clone)]
pub struct DialogTarget {
    pub controller: Entity<WorkbenchController>,
    pub window_id: WindowId,
    pub doc_id: DocumentId,
}

#[derive(Clone)]
struct Choice {
    value: usize,
    title: SharedString,
}

impl SelectItem for Choice {
    type Value = usize;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &usize {
        &self.value
    }
}

type ChoiceState = SelectState<SearchableVec<Choice>>;

fn make_select(
    labels: impl IntoIterator<Item = String>,
    selected: usize,
    searchable: bool,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ChoiceState> {
    let items: Vec<Choice> = labels
        .into_iter()
        .enumerate()
        .map(|(value, title)| Choice {
            value,
            title: title.into(),
        })
        .collect();
    let idx = selected.min(items.len().saturating_sub(1));
    cx.new(|cx| {
        SelectState::new(
            SearchableVec::new(items),
            Some(IndexPath::new(idx)),
            window,
            cx,
        )
        .searchable(searchable)
    })
}

fn column_labels(headers: &[String]) -> Vec<String> {
    headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{}: {h}", i + 1))
        .collect()
}

// =============================================================================
// Multi-column Sort dialog (1–16 levels, Text/Number/DateTime/Natural/Reverse/Shuffle)
// =============================================================================

const SORT_MODE_LABELS: [&str; 6] = [
    "Text",
    "Number",
    "Date / Time",
    "Natural",
    "Reverse Rows",
    "Shuffle Rows",
];
const SORT_DIR_LABELS: [&str; 2] = ["Ascending", "Descending"];

struct SortLevelState {
    id: usize,
    column: Entity<ChoiceState>,
    mode: Entity<ChoiceState>,
    direction: Entity<ChoiceState>,
    date_format: Entity<ChoiceState>,
    _subs: Vec<Subscription>,
}

struct SortDialogState {
    headers: Vec<String>,
    levels: Vec<SortLevelState>,
    next_level_id: usize,
    match_case: bool,
    error: Option<String>,
}

impl SortDialogState {
    fn add_level(&mut self, initial_col: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.levels.len() >= 16 {
            return;
        }
        let col = make_select(column_labels(&self.headers), initial_col, true, window, cx);
        let mode = make_select(SORT_MODE_LABELS.map(str::to_owned), 0, false, window, cx);
        let direction = make_select(SORT_DIR_LABELS.map(str::to_owned), 0, false, window, cx);
        let date_format = make_select(
            DateTimeFormat::ALL.map(|f| f.label().to_owned()),
            0,
            false,
            window,
            cx,
        );
        let subs = vec![
            cx.subscribe_in(
                &col,
                window,
                |_, _, _: &SelectEvent<SearchableVec<Choice>>, _, cx| cx.notify(),
            ),
            cx.subscribe_in(
                &mode,
                window,
                |_, _, _: &SelectEvent<SearchableVec<Choice>>, _, cx| cx.notify(),
            ),
            cx.subscribe_in(
                &direction,
                window,
                |_, _, _: &SelectEvent<SearchableVec<Choice>>, _, cx| cx.notify(),
            ),
            cx.subscribe_in(
                &date_format,
                window,
                |_, _, _: &SelectEvent<SearchableVec<Choice>>, _, cx| cx.notify(),
            ),
        ];
        let id = self.next_level_id;
        self.next_level_id += 1;
        self.levels.push(SortLevelState {
            id,
            column: col,
            mode,
            direction,
            date_format,
            _subs: subs,
        });
        cx.notify();
    }

    fn build_plan(&self, cx: &App) -> SortPlan {
        let case_sensitivity = if self.match_case {
            CaseSensitivity::Sensitive
        } else {
            CaseSensitivity::Insensitive
        };
        let keys: Vec<SortSpec> = self
            .levels
            .iter()
            .map(|level| {
                let column = *level.column.read(cx).selected_value().unwrap_or(&0);
                let mode_idx = *level.mode.read(cx).selected_value().unwrap_or(&0);
                let dir_idx = *level.direction.read(cx).selected_value().unwrap_or(&0);
                let mode = match mode_idx {
                    0 => SortMode::Text,
                    1 => SortMode::Number,
                    2 => {
                        let fmt_idx = *level.date_format.read(cx).selected_value().unwrap_or(&0);
                        SortMode::DateTime {
                            format: DateTimeFormat::ALL
                                .get(fmt_idx)
                                .copied()
                                .unwrap_or(DateTimeFormat::YearMonthDay),
                        }
                    }
                    3 => SortMode::Natural,
                    4 => SortMode::Reverse,
                    5 => SortMode::shuffle(),
                    _ => SortMode::Text,
                };
                let direction = if dir_idx == 1 {
                    SortDirection::Descending
                } else {
                    SortDirection::Ascending
                };
                SortSpec {
                    column,
                    direction,
                    mode,
                    case_sensitivity,
                }
            })
            .collect();
        SortPlan { keys }
    }
}

pub fn open_sort_dialog(
    target: DialogTarget,
    headers: Vec<String>,
    initial_col: usize,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let DialogTarget {
        controller,
        window_id,
        doc_id,
    } = target;
    let state = cx.new(|cx| {
        let mut st = SortDialogState {
            headers,
            levels: Vec::new(),
            next_level_id: 1,
            match_case: false,
            error: None,
        };
        st.add_level(initial_col, window, cx);
        st
    });
    let on_closed = std::rc::Rc::new(on_closed);
    controller.update(cx, |c, cx| c.set_window_editing(window_id, true, cx));
    window.open_dialog(cx, move |dialog, _, _cx| {
        let st_view = state.clone();
        let st_ok = state.clone();
        let ctrl_ok = controller.clone();
        let ctrl_view = controller.clone();
        let ctrl_close = controller.clone();
        let on_closed_cb = on_closed.clone();

        dialog
            .title("Sort Rows")
            .w(px(640.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                let st_read = st_view.read(cx);
                let plan = st_read.build_plan(cx);
                let estimate = ctrl_view.read(cx).sort_plan_disk_estimate(window_id, &plan);
                let error = st_read.error.clone();
                let match_case = st_read.match_case;
                let level_count = st_read.levels.len();
                let level_rows: Vec<_> = st_read
                    .levels
                    .iter()
                    .enumerate()
                    .map(|(ix, lvl)| {
                        let mode_idx = *lvl.mode.read(cx).selected_value().unwrap_or(&0);
                        let col_idx = *lvl.column.read(cx).selected_value().unwrap_or(&0);
                        let suggestions = if mode_idx == 2 {
                            ctrl_view
                                .read(cx)
                                .suggest_date_time_formats(window_id, col_idx)
                                .map(|s| s.matching_formats)
                                .unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                        (
                            ix,
                            lvl.id,
                            lvl.column.clone(),
                            lvl.mode.clone(),
                            lvl.direction.clone(),
                            lvl.date_format.clone(),
                            mode_idx,
                            suggestions,
                        )
                    })
                    .collect();

                let st_add = st_view.clone();
                let st_case = st_view.clone();
                content
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(level_rows.into_iter().map(
                        |(ix, id, col, mode, dir, date_fmt, mode_idx, suggestions)| {
                            let st_rm = st_view.clone();
                            let uses_column = mode_idx < 4;
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(22.))
                                                .text_size(px(12.))
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!("{}.", ix + 1)),
                                        )
                                        .child(
                                            div().w(px(180.)).child(
                                                Select::new(&col)
                                                    .id(("sort-col", id))
                                                    .small()
                                                    .disabled(!uses_column)
                                                    .accessibility_label(format!(
                                                        "Sort level {} column",
                                                        ix + 1
                                                    )),
                                            ),
                                        )
                                        .child(
                                            div().w(px(140.)).child(
                                                Select::new(&mode)
                                                    .id(("sort-mode", id))
                                                    .small()
                                                    .accessibility_label(format!(
                                                        "Sort level {} mode",
                                                        ix + 1
                                                    )),
                                            ),
                                        )
                                        .child(
                                            div().w(px(120.)).child(
                                                Select::new(&dir)
                                                    .id(("sort-dir", id))
                                                    .small()
                                                    .disabled(!uses_column)
                                                    .accessibility_label(format!(
                                                        "Sort level {} direction",
                                                        ix + 1
                                                    )),
                                            ),
                                        )
                                        .child(
                                            Button::new(("sort-remove", id))
                                                .label("×")
                                                .ghost()
                                                .small()
                                                .disabled(level_count <= 1)
                                                .on_click(move |_, _, cx| {
                                                    st_rm.update(cx, |s, cx| {
                                                        s.levels.retain(|l| l.id != id);
                                                        cx.notify();
                                                    });
                                                }),
                                        ),
                                )
                                .when(mode_idx == 2, |row| {
                                    let fmt_for_sugg = date_fmt.clone();
                                    row.child(
                                        div()
                                            .pl(px(30.))
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .child(
                                                        div()
                                                            .text_size(px(12.))
                                                            .text_color(cx.theme().muted_foreground)
                                                            .child("Format:"),
                                                    )
                                                    .child(
                                                        div().w(px(280.)).child(
                                                            Select::new(&date_fmt)
                                                                .id(("sort-date-fmt", id))
                                                                .small()
                                                                .accessibility_label(
                                                                    "Date-Time format",
                                                                ),
                                                        ),
                                                    ),
                                            )
                                            .when(!suggestions.is_empty(), |box_div| {
                                                box_div.child(
                                                    div()
                                                        .flex()
                                                        .flex_wrap()
                                                        .items_center()
                                                        .gap_1()
                                                        .child(
                                                            div()
                                                                .text_size(px(11.))
                                                                .text_color(
                                                                    cx.theme().muted_foreground,
                                                                )
                                                                .child("Detected:"),
                                                        )
                                                        .children(
                                                            suggestions
                                                                .into_iter()
                                                                .take(5)
                                                                .enumerate()
                                                                .map(|(s_ix, fmt)| {
                                                                    let fmt_target =
                                                                        fmt_for_sugg.clone();
                                                                    let fmt_idx = DateTimeFormat::ALL
                                                                        .iter()
                                                                        .position(|f| *f == fmt)
                                                                        .unwrap_or(0);
                                                                    Button::new((
                                                                        "date-fmt-sugg",
                                                                        id * 16 + s_ix,
                                                                    ))
                                                                    .label(fmt.label())
                                                                    .ghost()
                                                                    .xsmall()
                                                                    .on_click(
                                                                        move |_, window, cx| {
                                                                            fmt_target.update(
                                                                                cx,
                                                                                |sel, cx| {
                                                                                    sel.set_selected_value(
                                                                                        &fmt_idx,
                                                                                        window,
                                                                                        cx,
                                                                                    );
                                                                                },
                                                                            );
                                                                        },
                                                                    )
                                                                }),
                                                        ),
                                                )
                                            }),
                                    )
                                })
                        },
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .pt_1()
                            .child(
                                Button::new("add-sort-level")
                                    .label("+ Add sort level")
                                    .ghost()
                                    .small()
                                    .disabled(level_count >= 16)
                                    .on_click(move |_, window, cx| {
                                        st_add.update(cx, |s, cx| {
                                            let next_col = s
                                                .levels
                                                .len()
                                                .min(s.headers.len().saturating_sub(1));
                                            s.add_level(next_col, window, cx);
                                        });
                                    }),
                            )
                            .child(
                                Checkbox::new("sort-match-case")
                                    .label("Match case")
                                    .small()
                                    .checked(match_case)
                                    .on_change(move |checked, _, cx| {
                                        st_case.update(cx, |s, cx| {
                                            s.match_case = *checked;
                                            cx.notify();
                                        });
                                    }),
                            ),
                    )
                    .when_some(estimate, |box_div, bytes| {
                        box_div.child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "Estimated temporary space: {}",
                                    format_bytes(bytes)
                                )),
                        )
                    })
                    .when_some(error, |box_div, err| {
                        box_div.child(
                            div()
                                .id("sort-dialog-error")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(err),
                        )
                    })
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("cancel-sort").label("Cancel").small().on_click(
                        |_, window, cx| window.dispatch_action(Box::new(DialogCancel), cx),
                    ))
                    .child(
                        Button::new("apply-sort")
                            .label("Sort")
                            .primary()
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(DialogConfirm { secondary: false }),
                                    cx,
                                )
                            }),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let plan = st_ok.read(cx).build_plan(cx);
                let outcome =
                    ctrl_ok.update(cx, |c, cx| c.start_sort_active(window_id, doc_id, plan, cx));
                match outcome {
                    Ok(()) => true,
                    Err(err) => {
                        st_ok.update(cx, |s, cx| {
                            s.error = Some(err.text);
                            cx.notify();
                        });
                        window.refresh();
                        false
                    }
                }
            })
            .on_close(move |_, window, cx| {
                ctrl_close.update(cx, |c, cx| c.set_window_editing(window_id, false, cx));
                (on_closed_cb)(window, cx);
            })
    });
}

// =============================================================================
// Find & Remove Duplicates dialog
// =============================================================================

struct DuplicatesDialogState {
    headers: Vec<String>,
    all_fields: bool,
    selected: BTreeSet<usize>,
    match_case: bool,
    error: Option<String>,
}

pub fn open_duplicates_dialog(
    target: DialogTarget,
    headers: Vec<String>,
    initial_selected: Vec<usize>,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let DialogTarget {
        controller,
        window_id,
        doc_id,
    } = target;
    let default_all = initial_selected.is_empty();
    let state = cx.new(|_| DuplicatesDialogState {
        headers,
        all_fields: default_all,
        selected: initial_selected.into_iter().collect(),
        match_case: false,
        error: None,
    });
    let on_closed = std::rc::Rc::new(on_closed);
    controller.update(cx, |c, cx| c.set_window_editing(window_id, true, cx));
    window.open_dialog(cx, move |dialog, _, _cx| {
        let st_view = state.clone();
        let st_ok = state.clone();
        let ctrl_ok = controller.clone();
        let ctrl_close = controller.clone();
        let on_closed_cb = on_closed.clone();
        dialog
            .title("Find & Remove Duplicates")
            .w(px(460.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                let st = st_view.read(cx);
                let all_fields = st.all_fields;
                let match_case = st.match_case;
                let selected = st.selected.clone();
                let headers = st.headers.clone();
                let error = st.error.clone();
                let st_all = st_view.clone();
                let st_case = st_view.clone();
                content
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        Checkbox::new("dup-all-fields")
                            .label("Compare all fields (whole row)")
                            .small()
                            .checked(all_fields)
                            .on_change(move |checked, _, cx| {
                                st_all.update(cx, |s, cx| {
                                    s.all_fields = *checked;
                                    cx.notify();
                                });
                            }),
                    )
                    .when(!all_fields, |box_div| {
                        box_div.child(
                            div()
                                .id("dup-columns-list")
                                .max_h(px(180.))
                                .overflow_y_scroll()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .p_2()
                                .border_1()
                                .border_color(cx.theme().border)
                                .rounded(px(6.))
                                .children(headers.into_iter().enumerate().map(|(col_ix, name)| {
                                    let is_checked = selected.contains(&col_ix);
                                    let st_col = st_view.clone();
                                    Checkbox::new(("dup-col", col_ix))
                                        .label(format!("{}: {name}", col_ix + 1))
                                        .small()
                                        .checked(is_checked)
                                        .on_change(move |checked, _, cx| {
                                            st_col.update(cx, |s, cx| {
                                                if *checked {
                                                    s.selected.insert(col_ix);
                                                } else {
                                                    s.selected.remove(&col_ix);
                                                }
                                                cx.notify();
                                            });
                                        })
                                })),
                        )
                    })
                    .child(
                        Checkbox::new("dup-match-case")
                            .label("Match case")
                            .small()
                            .checked(match_case)
                            .on_change(move |checked, _, cx| {
                                st_case.update(cx, |s, cx| {
                                    s.match_case = *checked;
                                    cx.notify();
                                });
                            }),
                    )
                    .when_some(error, |box_div, err| {
                        box_div.child(
                            div()
                                .id("dup-dialog-error")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(err),
                        )
                    })
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel-duplicates")
                            .label("Cancel")
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DialogCancel), cx)
                            }),
                    )
                    .child(
                        Button::new("apply-duplicates")
                            .label("Find Duplicates")
                            .primary()
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(DialogConfirm { secondary: false }),
                                    cx,
                                )
                            }),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let (target, case) = {
                    let s = st_ok.read(cx);
                    let target = if s.all_fields {
                        DuplicateTarget::AllFields
                    } else {
                        DuplicateTarget::Columns(s.selected.iter().copied().collect())
                    };
                    let case = if s.match_case {
                        CaseSensitivity::Sensitive
                    } else {
                        CaseSensitivity::Insensitive
                    };
                    (target, case)
                };
                let res = ctrl_ok.update(cx, |c, cx| {
                    c.start_find_duplicates_active(window_id, doc_id, target, case, cx)
                });
                match res {
                    Ok(()) => true,
                    Err(err) => {
                        st_ok.update(cx, |s, cx| {
                            s.error = Some(err.text);
                            cx.notify();
                        });
                        window.refresh();
                        false
                    }
                }
            })
            .on_close(move |_, window, cx| {
                ctrl_close.update(cx, |c, cx| c.set_window_editing(window_id, false, cx));
                (on_closed_cb)(window, cx);
            })
    });
}

// =============================================================================
// Cleanup Selected Columns dialog
// =============================================================================

const CLEANUP_OPS: [&str; 5] = [
    "Trim leading/trailing whitespace",
    "Collapse repeated spaces",
    "Convert to UPPERCASE",
    "Convert to lowercase",
    "Fill empty cells",
];

struct CleanupDialogState {
    headers: Vec<String>,
    selected: BTreeSet<usize>,
    operation: Entity<ChoiceState>,
    fill_input: Entity<InputState>,
    error: Option<String>,
    _sub: Subscription,
}

pub fn open_cleanup_dialog(
    target: DialogTarget,
    headers: Vec<String>,
    initial_selected: Vec<usize>,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let DialogTarget {
        controller,
        window_id,
        doc_id,
    } = target;
    let state = cx.new(|cx| {
        let operation = make_select(CLEANUP_OPS.map(str::to_owned), 0, false, window, cx);
        let fill_input = cx.new(|cx| InputState::new(window, cx).placeholder("Replacement value"));
        let sub = cx.subscribe_in(
            &operation,
            window,
            |_, _, _: &SelectEvent<SearchableVec<Choice>>, _, cx| cx.notify(),
        );
        let mut selected: BTreeSet<usize> = initial_selected.into_iter().collect();
        if selected.is_empty() && !headers.is_empty() {
            selected.insert(0);
        }
        CleanupDialogState {
            headers,
            selected,
            operation,
            fill_input,
            error: None,
            _sub: sub,
        }
    });
    let on_closed = std::rc::Rc::new(on_closed);
    controller.update(cx, |c, cx| c.set_window_editing(window_id, true, cx));
    window.open_dialog(cx, move |dialog, _, _cx| {
        let st_view = state.clone();
        let st_ok = state.clone();
        let ctrl_ok = controller.clone();
        let ctrl_close = controller.clone();
        let on_closed_cb = on_closed.clone();
        dialog
            .title("Clean Selected Columns")
            .w(px(460.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                let st = st_view.read(cx);
                let op_idx = *st.operation.read(cx).selected_value().unwrap_or(&0);
                let op_select = st.operation.clone();
                let fill_input = st.fill_input.clone();
                let headers = st.headers.clone();
                let selected = st.selected.clone();
                let error = st.error.clone();
                content
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        Select::new(&op_select)
                            .id("cleanup-op")
                            .small()
                            .accessibility_label("Cleanup operation"),
                    )
                    .when(op_idx == 4, |box_div| {
                        box_div.child(
                            Input::new(&fill_input)
                                .small()
                                .aria_label("Fill empty value"),
                        )
                    })
                    .child(
                        div()
                            .id("cleanup-columns-list")
                            .max_h(px(180.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p_2()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded(px(6.))
                            .children(headers.into_iter().enumerate().map(|(col_ix, name)| {
                                let is_checked = selected.contains(&col_ix);
                                let st_col = st_view.clone();
                                Checkbox::new(("cleanup-col", col_ix))
                                    .label(format!("{}: {name}", col_ix + 1))
                                    .small()
                                    .checked(is_checked)
                                    .on_change(move |checked, _, cx| {
                                        st_col.update(cx, |s, cx| {
                                            if *checked {
                                                s.selected.insert(col_ix);
                                            } else {
                                                s.selected.remove(&col_ix);
                                            }
                                            cx.notify();
                                        });
                                    })
                            })),
                    )
                    .when_some(error, |box_div, err| {
                        box_div.child(
                            div()
                                .id("cleanup-dialog-error")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(err),
                        )
                    })
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel-cleanup")
                            .label("Cancel")
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DialogCancel), cx)
                            }),
                    )
                    .child(
                        Button::new("apply-cleanup")
                            .label("Clean")
                            .primary()
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(DialogConfirm { secondary: false }),
                                    cx,
                                )
                            }),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let cleanup = {
                    let s = st_ok.read(cx);
                    let op_idx = *s.operation.read(cx).selected_value().unwrap_or(&0);
                    let operation = match op_idx {
                        0 => CleanupOperation::Trim,
                        1 => CleanupOperation::CollapseSpaces,
                        2 => CleanupOperation::Uppercase,
                        3 => CleanupOperation::Lowercase,
                        4 => CleanupOperation::FillEmpty(
                            s.fill_input.read(cx).value().as_bytes().to_vec(),
                        ),
                        _ => CleanupOperation::Trim,
                    };
                    ColumnCleanup {
                        columns: s.selected.iter().copied().collect(),
                        operation,
                    }
                };
                let res = ctrl_ok.update(cx, |c, cx| {
                    c.start_cleanup_active(window_id, doc_id, cleanup, cx)
                });
                match res {
                    Ok(()) => true,
                    Err(err) => {
                        st_ok.update(cx, |s, cx| {
                            s.error = Some(err.text);
                            cx.notify();
                        });
                        window.refresh();
                        false
                    }
                }
            })
            .on_close(move |_, window, cx| {
                ctrl_close.update(cx, |c, cx| c.set_window_editing(window_id, false, cx));
                (on_closed_cb)(window, cx);
            })
    });
}

// =============================================================================
// Insert Rows / Insert Columns dialogs
// =============================================================================

#[derive(Default)]
struct DialogError {
    error: Option<String>,
}

pub fn open_insert_dialog(
    target: DialogTarget,
    insert_rows: bool,
    default_position: u64,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let DialogTarget {
        controller,
        window_id,
        doc_id,
    } = target;
    let count_input = cx.new(|cx| InputState::new(window, cx).default_value("1"));
    let pos_input =
        cx.new(|cx| InputState::new(window, cx).default_value(default_position.max(1).to_string()));
    let err_state = cx.new(|_| DialogError::default());
    let on_closed = std::rc::Rc::new(on_closed);
    controller.update(cx, |c, cx| c.set_window_editing(window_id, true, cx));
    window.open_dialog(cx, move |dialog, _, cx| {
        let count_v = count_input.clone();
        let pos_v = pos_input.clone();
        let count_ok = count_input.clone();
        let pos_ok = pos_input.clone();
        let err_ok = err_state.clone();
        let error = err_state.read(cx).error.clone();
        let ctrl_ok = controller.clone();
        let ctrl_close = controller.clone();
        let on_closed_cb = on_closed.clone();
        dialog
            .title(if insert_rows {
                "Insert Rows"
            } else {
                "Insert Columns"
            })
            .w(px(380.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                content
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(110.)).child("Count:"))
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&count_v).small().aria_label("Count")),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(110.)).child(if insert_rows {
                                "At row (1-based):"
                            } else {
                                "At column (1-based):"
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&pos_v).small().aria_label("Position")),
                            ),
                    )
                    .when_some(error.clone(), |box_div, err| {
                        box_div.child(
                            div()
                                .id("insert-dialog-error")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(err),
                        )
                    })
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel-insert")
                            .label("Cancel")
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DialogCancel), cx)
                            }),
                    )
                    .child(
                        Button::new("apply-insert")
                            .label("Insert")
                            .primary()
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(DialogConfirm { secondary: false }),
                                    cx,
                                )
                            }),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let mut dlg = if insert_rows {
                    StructuralDialog::insert_rows(0)
                } else {
                    StructuralDialog::insert_columns(0)
                };
                dlg.count = count_ok.read(cx).value().to_string();
                dlg.position = pos_ok.read(cx).value().to_string();
                let res = ctrl_ok.update(cx, |c, cx| {
                    if insert_rows {
                        c.start_insert_rows_active(window_id, doc_id, &dlg, cx)
                    } else {
                        c.start_insert_columns_active(window_id, doc_id, &dlg, cx)
                    }
                });
                match res {
                    Ok(()) => true,
                    Err(err) => {
                        err_ok.update(cx, |s, cx| {
                            s.error = Some(err.text);
                            cx.notify();
                        });
                        window.refresh();
                        false
                    }
                }
            })
            .on_close(move |_, window, cx| {
                ctrl_close.update(cx, |c, cx| c.set_window_editing(window_id, false, cx));
                (on_closed_cb)(window, cx);
            })
    });
}

// =============================================================================
// Move Columns dialog
// =============================================================================

struct MoveColumnsState {
    headers: Vec<String>,
    selected: BTreeSet<usize>,
    position: Entity<InputState>,
    error: Option<String>,
}

pub fn open_move_columns_dialog(
    target: DialogTarget,
    headers: Vec<String>,
    initial_selected: Vec<usize>,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let DialogTarget {
        controller,
        window_id,
        doc_id,
    } = target;
    let state = cx.new(|cx| {
        let mut selected: BTreeSet<usize> = initial_selected.into_iter().collect();
        if selected.is_empty() && !headers.is_empty() {
            selected.insert(0);
        }
        MoveColumnsState {
            headers,
            selected,
            position: cx.new(|cx| InputState::new(window, cx).default_value("1")),
            error: None,
        }
    });
    let on_closed = std::rc::Rc::new(on_closed);
    controller.update(cx, |c, cx| c.set_window_editing(window_id, true, cx));
    window.open_dialog(cx, move |dialog, _, _cx| {
        let st_view = state.clone();
        let st_ok = state.clone();
        let ctrl_ok = controller.clone();
        let ctrl_close = controller.clone();
        let on_closed_cb = on_closed.clone();
        dialog
            .title("Move Columns")
            .w(px(440.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                let st = st_view.read(cx);
                let headers = st.headers.clone();
                let selected = st.selected.clone();
                let pos_input = st.position.clone();
                let error = st.error.clone();
                content
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .id("move-columns-list")
                            .max_h(px(180.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p_2()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded(px(6.))
                            .children(headers.into_iter().enumerate().map(|(col_ix, name)| {
                                let is_checked = selected.contains(&col_ix);
                                let st_col = st_view.clone();
                                Checkbox::new(("move-col", col_ix))
                                    .label(format!("{}: {name}", col_ix + 1))
                                    .small()
                                    .checked(is_checked)
                                    .on_change(move |checked, _, cx| {
                                        st_col.update(cx, |s, cx| {
                                            if *checked {
                                                s.selected.insert(col_ix);
                                            } else {
                                                s.selected.remove(&col_ix);
                                            }
                                            cx.notify();
                                        });
                                    })
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(150.)).child("Destination (1-based):"))
                            .child(
                                div().flex_1().child(
                                    Input::new(&pos_input)
                                        .small()
                                        .aria_label("Destination position"),
                                ),
                            ),
                    )
                    .when_some(error, |box_div, err| {
                        box_div.child(
                            div()
                                .id("move-dialog-error")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(err),
                        )
                    })
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("cancel-move").label("Cancel").small().on_click(
                        |_, window, cx| window.dispatch_action(Box::new(DialogCancel), cx),
                    ))
                    .child(
                        Button::new("apply-move")
                            .label("Move")
                            .primary()
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(DialogConfirm { secondary: false }),
                                    cx,
                                )
                            }),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let (columns, pos_str) = {
                    let s = st_ok.read(cx);
                    (
                        s.selected.iter().copied().collect::<Vec<_>>(),
                        s.position.read(cx).value().trim().to_string(),
                    )
                };
                let Ok(one_based) = pos_str.parse::<usize>() else {
                    st_ok.update(cx, |s, cx| {
                        s.error = Some("Enter a positive 1-based column position.".into());
                        cx.notify();
                    });
                    window.refresh();
                    return false;
                };
                if one_based == 0 {
                    st_ok.update(cx, |s, cx| {
                        s.error = Some("Destination position starts at 1.".into());
                        cx.notify();
                    });
                    window.refresh();
                    return false;
                }
                let res = ctrl_ok.update(cx, |c, cx| {
                    c.start_move_columns_active(window_id, doc_id, columns, one_based - 1, cx)
                });
                match res {
                    Ok(()) => true,
                    Err(err) => {
                        st_ok.update(cx, |s, cx| {
                            s.error = Some(err.text);
                            cx.notify();
                        });
                        window.refresh();
                        false
                    }
                }
            })
            .on_close(move |_, window, cx| {
                ctrl_close.update(cx, |c, cx| c.set_window_editing(window_id, false, cx));
                (on_closed_cb)(window, cx);
            })
    });
}

// =============================================================================
// Split Column dialog
// =============================================================================

pub fn open_split_column_dialog(
    target: DialogTarget,
    headers: Vec<String>,
    initial_col: usize,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let DialogTarget {
        controller,
        window_id,
        doc_id,
    } = target;
    let col_select = make_select(column_labels(&headers), initial_col, true, window, cx);
    let sep_input = cx.new(|cx| {
        InputState::new(window, cx)
            .default_value("-")
            .placeholder("Separator")
    });
    let err_state = cx.new(|_| DialogError::default());
    let on_closed = std::rc::Rc::new(on_closed);
    controller.update(cx, |c, cx| c.set_window_editing(window_id, true, cx));
    window.open_dialog(cx, move |dialog, _, cx| {
        let col_v = col_select.clone();
        let sep_v = sep_input.clone();
        let col_ok = col_select.clone();
        let sep_ok = sep_input.clone();
        let err_ok = err_state.clone();
        let error = err_state.read(cx).error.clone();
        let ctrl_ok = controller.clone();
        let ctrl_close = controller.clone();
        let on_closed_cb = on_closed.clone();
        dialog
            .title("Split Column")
            .w(px(420.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                content
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        Select::new(&col_v)
                            .id("split-column-select")
                            .small()
                            .accessibility_label("Column to split"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(90.)).child("Separator:"))
                            .child(
                                div().flex_1().child(
                                    Input::new(&sep_v).small().aria_label("Split separator"),
                                ),
                            ),
                    )
                    .when_some(error.clone(), |box_div, err| {
                        box_div.child(
                            div()
                                .id("split-dialog-error")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(err),
                        )
                    })
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel-split")
                            .label("Cancel")
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DialogCancel), cx)
                            }),
                    )
                    .child(
                        Button::new("apply-split")
                            .label("Split")
                            .primary()
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(DialogConfirm { secondary: false }),
                                    cx,
                                )
                            }),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let col = *col_ok.read(cx).selected_value().unwrap_or(&0);
                let sep = sep_ok.read(cx).value().as_bytes().to_vec();
                let res = ctrl_ok.update(cx, |c, cx| {
                    c.start_split_column_active(window_id, doc_id, col, sep, cx)
                });
                match res {
                    Ok(()) => true,
                    Err(err) => {
                        err_ok.update(cx, |s, cx| {
                            s.error = Some(err.text);
                            cx.notify();
                        });
                        window.refresh();
                        false
                    }
                }
            })
            .on_close(move |_, window, cx| {
                ctrl_close.update(cx, |c, cx| c.set_window_editing(window_id, false, cx));
                (on_closed_cb)(window, cx);
            })
    });
}

// =============================================================================
// Combine Columns dialog
// =============================================================================

struct CombineColumnsState {
    headers: Vec<String>,
    selected_order: Vec<usize>,
    separator: Entity<InputState>,
    error: Option<String>,
}

pub fn open_combine_columns_dialog(
    target: DialogTarget,
    headers: Vec<String>,
    initial_selected: Vec<usize>,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let DialogTarget {
        controller,
        window_id,
        doc_id,
    } = target;
    let state = cx.new(|cx| {
        let mut selected_order = initial_selected;
        if selected_order.len() < 2 && headers.len() >= 2 {
            selected_order = vec![0, 1];
        }
        CombineColumnsState {
            headers,
            selected_order,
            separator: cx.new(|cx| InputState::new(window, cx).default_value(" ")),
            error: None,
        }
    });
    let on_closed = std::rc::Rc::new(on_closed);
    controller.update(cx, |c, cx| c.set_window_editing(window_id, true, cx));
    window.open_dialog(cx, move |dialog, _, _cx| {
        let st_view = state.clone();
        let st_ok = state.clone();
        let ctrl_ok = controller.clone();
        let ctrl_close = controller.clone();
        let on_closed_cb = on_closed.clone();
        dialog
            .title("Combine Columns")
            .w(px(440.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                let st = st_view.read(cx);
                let headers = st.headers.clone();
                let selected_order = st.selected_order.clone();
                let sep_input = st.separator.clone();
                let error = st.error.clone();
                content
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .id("combine-columns-list")
                            .max_h(px(180.))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p_2()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded(px(6.))
                            .children(headers.into_iter().enumerate().map(|(col_ix, name)| {
                                let is_checked = selected_order.contains(&col_ix);
                                let st_col = st_view.clone();
                                Checkbox::new(("combine-col", col_ix))
                                    .label(format!("{}: {name}", col_ix + 1))
                                    .small()
                                    .checked(is_checked)
                                    .on_change(move |checked, _, cx| {
                                        st_col.update(cx, |s, cx| {
                                            if *checked {
                                                if !s.selected_order.contains(&col_ix) {
                                                    s.selected_order.push(col_ix);
                                                }
                                            } else {
                                                s.selected_order.retain(|&c| c != col_ix);
                                            }
                                            cx.notify();
                                        });
                                    })
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(90.)).child("Separator:"))
                            .child(
                                div().flex_1().child(
                                    Input::new(&sep_input)
                                        .small()
                                        .aria_label("Combine separator"),
                                ),
                            ),
                    )
                    .when_some(error, |box_div, err| {
                        box_div.child(
                            div()
                                .id("combine-dialog-error")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(err),
                        )
                    })
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel-combine")
                            .label("Cancel")
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DialogCancel), cx)
                            }),
                    )
                    .child(
                        Button::new("apply-combine")
                            .label("Combine")
                            .primary()
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(DialogConfirm { secondary: false }),
                                    cx,
                                )
                            }),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let (columns, sep) = {
                    let s = st_ok.read(cx);
                    (
                        s.selected_order.clone(),
                        s.separator.read(cx).value().as_bytes().to_vec(),
                    )
                };
                let res = ctrl_ok.update(cx, |c, cx| {
                    c.start_combine_columns_active(window_id, doc_id, columns, sep, cx)
                });
                match res {
                    Ok(()) => true,
                    Err(err) => {
                        st_ok.update(cx, |s, cx| {
                            s.error = Some(err.text);
                            cx.notify();
                        });
                        window.refresh();
                        false
                    }
                }
            })
            .on_close(move |_, window, cx| {
                ctrl_close.update(cx, |c, cx| c.set_window_editing(window_id, false, cx));
                (on_closed_cb)(window, cx);
            })
    });
}
