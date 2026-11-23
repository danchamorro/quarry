//! Thin GPUI modal dialogs for Phase 4B-2c workflows:
//! - Combine Files
//! - 4-mode Join Files (Left, Inner, Right, Full Outer) with open-tab `DocumentLease`s
//! - Inspect File & Recovery
//! - Temporary Storage & Sidecar Index Cache
//! - About Quarry

use std::path::PathBuf;

use gpui_kit::component::{
    ActiveTheme, Disableable, IndexPath, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    dialog::Cancel as DialogCancel,
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
};
use gpui_kit::{prelude::*, *};
use quarry_core::{Encoding, HeaderMode, JoinMode, JoinSource};
use quarry_workbench::{
    ADAPTIVE_MEMORY_BUDGET_BYTES, DelimiterMode, JoinSideSource, WindowId, format_bytes,
};

use crate::menus::encoding_from_label;
use crate::workbench::WorkbenchController;

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
    })
}

const DELIMITER_LABELS: [&str; 5] = [
    "Auto",
    "Comma (,)",
    "Tab (\\t)",
    "Pipe (|)",
    "Semicolon (;)",
];
const ENCODING_LABELS: [&str; 6] = [
    "Auto",
    "UTF-8",
    "UTF-16 LE",
    "UTF-16 BE",
    "Windows-1252",
    "ISO-8859-1",
];
const HEADER_MODE_LABELS: [&str; 3] = ["Auto", "First row is header", "No header row"];

fn delimiter_from_index(idx: usize) -> DelimiterMode {
    match idx {
        1 => DelimiterMode::Comma,
        2 => DelimiterMode::Tab,
        3 => DelimiterMode::Pipe,
        4 => DelimiterMode::Semicolon,
        _ => DelimiterMode::Auto,
    }
}

fn delimiter_to_index(mode: DelimiterMode) -> usize {
    match mode {
        DelimiterMode::Auto => 0,
        DelimiterMode::Comma => 1,
        DelimiterMode::Tab => 2,
        DelimiterMode::Pipe => 3,
        DelimiterMode::Semicolon => 4,
    }
}

fn header_mode_from_index(idx: usize) -> HeaderMode {
    match idx {
        1 => HeaderMode::FirstRow,
        2 => HeaderMode::NoHeader,
        _ => HeaderMode::Auto,
    }
}

fn header_mode_to_index(mode: HeaderMode) -> usize {
    match mode {
        HeaderMode::Auto => 0,
        HeaderMode::FirstRow => 1,
        HeaderMode::NoHeader => 2,
    }
}

fn encoding_to_index(encoding: Option<Encoding>) -> usize {
    match encoding {
        None => 0,
        Some(Encoding::Utf8) => 1,
        Some(Encoding::Utf16Le) => 2,
        Some(Encoding::Utf16Be) => 3,
        Some(Encoding::Windows1252) => 4,
        Some(Encoding::Latin1) => 5,
    }
}

// =============================================================================
// 1. Combine Files Dialog
// =============================================================================

struct CombineDialogState {
    controller: Entity<WorkbenchController>,
    delimiter: Entity<ChoiceState>,
    encoding: Entity<ChoiceState>,
    _subs: Vec<Subscription>,
}

pub fn open_combine_dialog(
    controller: Entity<WorkbenchController>,
    on_open_output: impl Fn(PathBuf, &mut Window, &mut App) + 'static,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let (init_delim, init_enc) = {
        let c = controller.read(cx);
        let wf = c.combine();
        (
            delimiter_to_index(wf.delimiter),
            encoding_to_index(wf.encoding),
        )
    };
    let delimiter = make_select(DELIMITER_LABELS.map(str::to_owned), init_delim, window, cx);
    let encoding = make_select(ENCODING_LABELS.map(str::to_owned), init_enc, window, cx);
    let state = cx.new(|cx| {
        let c1 = controller.clone();
        let c2 = controller.clone();
        let delim_for_enc = delimiter.clone();
        let enc_for_delim = encoding.clone();
        let subs = vec![
            cx.observe(&controller, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &delimiter,
                window,
                move |_, sel, _: &SelectEvent<SearchableVec<Choice>>, _, cx| {
                    let d = delimiter_from_index(*sel.read(cx).selected_value().unwrap_or(&0));
                    let e =
                        encoding_from_label(*enc_for_delim.read(cx).selected_value().unwrap_or(&0));
                    let hdr = c1.read(cx).combine().has_header;
                    c1.update(cx, |c, cx| c.combine_set_options(d, hdr, e, cx));
                },
            ),
            cx.subscribe_in(
                &encoding,
                window,
                move |_, sel, _: &SelectEvent<SearchableVec<Choice>>, _, cx| {
                    let d = delimiter_from_index(
                        *delim_for_enc.read(cx).selected_value().unwrap_or(&0),
                    );
                    let e = encoding_from_label(*sel.read(cx).selected_value().unwrap_or(&0));
                    let hdr = c2.read(cx).combine().has_header;
                    c2.update(cx, |c, cx| c.combine_set_options(d, hdr, e, cx));
                },
            ),
        ];
        CombineDialogState {
            controller: controller.clone(),
            delimiter: delimiter.clone(),
            encoding: encoding.clone(),
            _subs: subs,
        }
    });

    let open_output_cb = std::rc::Rc::new(on_open_output);
    let on_closed = std::rc::Rc::new(on_closed);
    window.open_dialog(cx, move |dialog, _, cx| {
        let on_closed = on_closed.clone();
        let state_body = state.clone();
        let controller_footer = controller.clone();
        let open_output = open_output_cb.clone();
        let wf = controller.read(cx).combine();
        let busy = wf.busy();
        let has_files = wf.paths.len() >= 2;
        let can_choose_dest = !busy && wf.plan.is_some();
        let can_write =
            !busy && wf.plan.is_some() && wf.space.is_some() && wf.destination.is_some();
        let finished_path = wf.result.as_ref().map(|r| r.destination.clone());

        let c_check = controller_footer.clone();
        let c_dest = controller_footer.clone();
        let c_write = controller_footer.clone();
        let c_cancel = controller_footer.clone();

        dialog
            .title("Combine Files")
            .w(px(620.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                let st = state_body.read(cx);
                let ctrl = st.controller.clone();
                let wf = ctrl.read(cx).combine();
                let paths = wf.paths.clone();
                let has_header = wf.has_header;
                let busy = wf.busy();
                let message = wf.message.clone();
                let plan_summary = wf.plan.as_ref().map(|p| {
                    format!(
                        "{} files · {} columns · {} data rows · ~{} output",
                        p.inputs.len(),
                        p.columns,
                        p.data_rows,
                        format_bytes(p.output_bytes)
                    )
                });
                let dest_summary = wf.destination.as_ref().map(|d| {
                    let space_note = wf
                        .space
                        .as_ref()
                        .map(|s| format!(" ({} free)", format_bytes(s.available_bytes)))
                        .unwrap_or_else(|| " (checking disk space…)".into());
                    format!("Output: {}{}", d.display(), space_note)
                });
                let progress_text = wf
                    .check_progress()
                    .map(|p| {
                        format!(
                            "Checking file {} of {}…",
                            (p.file_index + 1).min(p.files.max(1)),
                            p.files
                        )
                    })
                    .or_else(|| {
                        wf.write_progress().map(|p| {
                            format!(
                                "Combining file {} of {}…",
                                (p.file_index + 1).min(p.files.max(1)),
                                p.files
                            )
                        })
                    });
                let result_text = wf.result.as_ref().map(|r| {
                    format!(
                        "Combined {} rows ({}) to {}",
                        r.data_rows,
                        format_bytes(r.bytes_written),
                        r.destination.display()
                    )
                });

                let c_add = ctrl.clone();
                let c_hdr = ctrl.clone();
                let delim_ent = st.delimiter.clone();
                let enc_ent = st.encoding.clone();

                let body = div()
                    .id("combine-dialog-scroll")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .max_h(px(380.))
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "Select 2 or more files with matching headers and columns.",
                                    ),
                            )
                            .child(
                                Button::new("combine-add-files")
                                    .label("+ Add Files…")
                                    .small()
                                    .disabled(busy)
                                    .on_click(move |_, _, cx| {
                                        let sel = cx.prompt_for_paths(PathPromptOptions {
                                            files: true,
                                            directories: false,
                                            multiple: true,
                                            prompt: Some("Select files to combine".into()),
                                        });
                                        let c_add = c_add.clone();
                                        cx.spawn(async move |cx| {
                                            if let Ok(Ok(Some(chosen))) = sel.await {
                                                cx.update(|cx| {
                                                    c_add.update(cx, |c, cx| {
                                                        c.combine_add_paths(chosen, cx);
                                                    });
                                                });
                                            }
                                        })
                                        .detach();
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded(px(4.))
                            .p_2()
                            .min_h(px(72.))
                            .when(paths.is_empty(), |list| {
                                list.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child("No files added yet."),
                                )
                            })
                            .children(paths.iter().enumerate().map(|(ix, path)| {
                                let c_up = ctrl.clone();
                                let c_dn = ctrl.clone();
                                let c_rm = ctrl.clone();
                                let total = paths.len();
                                let label = format!(
                                    "{}. {}",
                                    ix + 1,
                                    path.file_name()
                                        .unwrap_or(path.as_os_str())
                                        .to_string_lossy()
                                );
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().flex_1().truncate().child(label))
                                    .child(
                                        Button::new(("combine-up", ix))
                                            .label("↑")
                                            .ghost()
                                            .xsmall()
                                            .disabled(busy || ix == 0)
                                            .on_click(move |_, _, cx| {
                                                c_up.update(cx, |c, cx| {
                                                    c.combine_move_path(ix, ix - 1, cx);
                                                });
                                            }),
                                    )
                                    .child(
                                        Button::new(("combine-dn", ix))
                                            .label("↓")
                                            .ghost()
                                            .xsmall()
                                            .disabled(busy || ix + 1 >= total)
                                            .on_click(move |_, _, cx| {
                                                c_dn.update(cx, |c, cx| {
                                                    c.combine_move_path(ix, ix + 1, cx);
                                                });
                                            }),
                                    )
                                    .child(
                                        Button::new(("combine-rm", ix))
                                            .label("×")
                                            .ghost()
                                            .xsmall()
                                            .disabled(busy)
                                            .on_click(move |_, _, cx| {
                                                c_rm.update(cx, |c, cx| {
                                                    c.combine_remove_path(ix, cx);
                                                });
                                            }),
                                    )
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(140.))
                                    .child(Select::new(&delim_ent).small().disabled(busy)),
                            )
                            .child(
                                div()
                                    .w(px(150.))
                                    .child(Select::new(&enc_ent).small().disabled(busy)),
                            )
                            .child(
                                Checkbox::new("combine-has-header")
                                    .label("First row is header")
                                    .checked(has_header)
                                    .disabled(busy)
                                    .on_click(move |checked, _, cx| {
                                        let d = c_hdr.read(cx).combine().delimiter;
                                        let e = c_hdr.read(cx).combine().encoding;
                                        c_hdr.update(cx, |c, cx| {
                                            c.combine_set_options(d, *checked, e, cx);
                                        });
                                    }),
                            ),
                    )
                    .when_some(progress_text, |el, text| {
                        el.child(
                            div()
                                .id("combine-progress")
                                .role(Role::Status)
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(text),
                        )
                    })
                    .when_some(plan_summary, |el, summary| {
                        el.child(
                            div()
                                .id("combine-plan-summary")
                                .text_size(px(12.))
                                .child(summary),
                        )
                    })
                    .when_some(dest_summary, |el, summary| {
                        el.child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(summary),
                        )
                    })
                    .when_some(result_text, |el, text| {
                        el.child(
                            div()
                                .id("combine-result")
                                .role(Role::Status)
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(text),
                        )
                    })
                    .when_some(message, |el, msg| {
                        el.child(
                            div()
                                .id("combine-message")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(msg),
                        )
                    });
                content.child(body)
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .when(busy, |f| {
                        f.child(
                            Button::new("combine-cancel-job")
                                .label("Cancel Job")
                                .danger()
                                .small()
                                .on_click(move |_, _, cx| {
                                    c_cancel.update(cx, |c, cx| c.combine_cancel(cx));
                                }),
                        )
                    })
                    .child(
                        Button::new("combine-close")
                            .label("Close")
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DialogCancel), cx);
                            }),
                    )
                    .child(
                        Button::new("combine-check")
                            .label("Check Files")
                            .small()
                            .disabled(busy || !has_files)
                            .on_click(move |_, _, cx| {
                                c_check.update(cx, |c, cx| c.combine_start_check(cx));
                            }),
                    )
                    .child(
                        Button::new("combine-choose-dest")
                            .label("Choose Output…")
                            .small()
                            .disabled(!can_choose_dest)
                            .on_click(move |_, _, cx| {
                                let default_dir = c_dest
                                    .read(cx)
                                    .combine()
                                    .paths
                                    .first()
                                    .and_then(|p| p.parent())
                                    .unwrap_or_else(|| std::path::Path::new("."))
                                    .to_path_buf();
                                let sel =
                                    cx.prompt_for_new_path(&default_dir, Some("combined.csv"));
                                let c_dest = c_dest.clone();
                                cx.spawn(async move |cx| {
                                    if let Ok(Ok(Some(path))) = sel.await {
                                        cx.update(|cx| {
                                            c_dest.update(cx, |c, cx| {
                                                c.combine_prepare_destination(path, cx);
                                            });
                                        });
                                    }
                                })
                                .detach();
                            }),
                    )
                    .child(
                        Button::new("combine-run")
                            .label("Combine")
                            .primary()
                            .small()
                            .disabled(!can_write)
                            .on_click(move |_, _, cx| {
                                c_write.update(cx, |c, cx| c.combine_start_write(cx));
                            }),
                    )
                    .when_some(finished_path, |f, out_path| {
                        let open_output = open_output.clone();
                        f.child(
                            Button::new("combine-open-result")
                                .label("Open Combined File")
                                .primary()
                                .small()
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    open_output(out_path.clone(), window, cx);
                                }),
                        )
                    }),
            )
            .on_close(move |_, window, cx| {
                on_closed(window, cx);
            })
    });
}

// =============================================================================
// 2. 4-mode Join Files Dialog (Left / Inner / Right / Full Outer)
// =============================================================================

const JOIN_MODES: [(JoinMode, &str); 4] = [
    (JoinMode::Left, "Left"),
    (JoinMode::Inner, "Inner"),
    (JoinMode::Right, "Right"),
    (JoinMode::Full, "Full Outer"),
];

pub fn open_join_dialog(
    controller: Entity<WorkbenchController>,
    on_open_output: impl Fn(PathBuf, &mut Window, &mut App) + 'static,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let observer = cx.new(|cx| {
        let sub = cx.observe(&controller, |_, _, cx| cx.notify());
        (controller.clone(), sub)
    });
    let open_output_cb = std::rc::Rc::new(on_open_output);
    let on_closed = std::rc::Rc::new(on_closed);
    let controller_close = controller.clone();

    window.open_dialog(cx, move |dialog, _, cx| {
        let on_closed = on_closed.clone();
        let _obs = observer.read(cx);
        let c_body = controller.clone();
        let c_footer = controller.clone();
        let c_close = controller_close.clone();
        let open_output = open_output_cb.clone();

        let (busy, ready_to_analyze, has_plan, can_write, finished_path) = {
            let wf = controller.read(cx).join();
            (
                wf.busy(),
                wf.ready_to_analyze(),
                wf.plan.is_some(),
                !wf.busy()
                    && wf.plan.is_some()
                    && wf.destination.is_some()
                    && wf.confirmations_complete(),
                wf.result.as_ref().map(|r| r.destination.clone()),
            )
        };

        let c_preview = c_footer.clone();
        let c_dest = c_footer.clone();
        let c_write = c_footer.clone();
        let c_cancel = c_footer.clone();
        let c_reset = c_footer.clone();

        dialog
            .title("Join Files")
            .w(px(720.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                let open_tabs = c_body.read(cx).join_open_tab_choices();
                let wf = c_body.read(cx).join();
                let busy = wf.busy();
                let mode = wf.mode;
                let ignore_case = wf.ignore_ascii_case;
                let req_unique = wf.require_unique;
                let side_has_header = [wf.formats[0].has_header, wf.formats[1].has_header];
                let side_is_open_tab = [
                    matches!(wf.sides[0], Some(JoinSideSource::OpenTab { .. })),
                    matches!(wf.sides[1], Some(JoinSideSource::OpenTab { .. })),
                ];
                let keys = wf.keys.clone();
                let out_cols = wf.output_columns.clone();
                let side_labels: [String; 2] = [0, 1].map(|s| match wf.sides[s].as_ref() {
                    None => "Not selected".into(),
                    Some(JoinSideSource::File(p)) => p
                        .file_name()
                        .unwrap_or(p.as_os_str())
                        .to_string_lossy()
                        .into_owned(),
                    Some(JoinSideSource::OpenTab { label, .. }) => format!("{label} (open tab)"),
                });
                let side_col_names: [Vec<String>; 2] = [0, 1].map(|s| {
                    wf.headers[s]
                        .as_ref()
                        .map(|h| h.columns.clone())
                        .unwrap_or_default()
                });
                let needs_expansion = wf.needs_expansion_confirmation();
                let needs_layout = wf.needs_layout_confirmation();
                let ack_expansion = wf.acknowledge_expansion;
                let ack_layout = wf.acknowledge_layout;
                let plan_stats = wf.plan.as_ref().map(|plan| {
                    let a = plan.analysis();
                    (
                        format!(
                            "Output: {} rows (~{}) · Matched: {} main / {} additional · Unmatched: {} / {} · Many-to-many groups: {}",
                            a.output_rows,
                            format_bytes(a.output_bytes),
                            a.matching_rows[0],
                            a.matching_rows[1],
                            a.unmatched_rows[0],
                            a.unmatched_rows[1],
                            a.many_to_many_groups
                        ),
                        a.preview
                            .iter()
                            .take(4)
                            .map(|row| row.join(" | "))
                            .collect::<Vec<_>>(),
                    )
                });
                let dest_label = wf
                    .destination
                    .as_ref()
                    .map(|p| format!("Output: {}", p.display()));
                let progress_text = wf
                    .analysis_progress()
                    .map(|p| format!("Preview: {}…", p.phase.label()))
                    .or_else(|| {
                        wf.write_progress()
                            .map(|p| format!("Creating joined file: {}…", p.phase.label()))
                    });
                let result_text = wf.result.as_ref().map(|r| {
                    format!(
                        "Created joined file with {} rows ({}) at {}",
                        r.data_rows,
                        format_bytes(r.bytes_written),
                        r.destination.display()
                    )
                });
                let message = wf.message.clone();

                let body = div()
                    .id("join-dialog-scroll")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .max_h(px(440.))
                    .overflow_y_scroll()
                    // Side pickers (Main & Additional)
                    .children([0usize, 1usize].into_iter().map(|side| {
                        let c_browse = c_body.clone();
                        let c_clear = c_body.clone();
                        let c_unique = c_body.clone();
                        let title = if side == 0 { "Main input" } else { "Additional input" };
                        let tabs_for_side = open_tabs.clone();
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p_2()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded(px(4.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .w(px(120.))
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .truncate()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(side_labels[side].clone()),
                                    )
                                    .child(
                                        Button::new(("join-browse", side))
                                            .label("Browse File…")
                                            .small()
                                            .disabled(busy)
                                            .on_click(move |_, _, cx| {
                                                let sel = cx.prompt_for_paths(PathPromptOptions {
                                                    files: true,
                                                    directories: false,
                                                    multiple: false,
                                                    prompt: Some(format!("Select {title}").into()),
                                                });
                                                let c_browse = c_browse.clone();
                                                cx.spawn(async move |cx| {
                                                    if let Ok(Ok(Some(paths))) = sel.await
                                                        && let Some(path) = paths.into_iter().next()
                                                    {
                                                        cx.update(|cx| {
                                                            c_browse.update(cx, |c, cx| {
                                                                let _ = c.join_set_file_side(side, path, cx);
                                                            });
                                                        });
                                                    }
                                                })
                                                .detach();
                                            }),
                                    )
                                    .child(
                                        Button::new(("join-clear", side))
                                            .label("Clear")
                                            .ghost()
                                            .xsmall()
                                            .disabled(busy)
                                            .on_click(move |_, _, cx| {
                                                c_clear.update(cx, |c, cx| {
                                                    c.join_clear_side(side, cx);
                                                });
                                            }),
                                    ),
                            )
                            .when(!tabs_for_side.is_empty(), |row| {
                                row.child(
                                    div()
                                        .id(("join-open-tabs-scroll", side))
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap_1()
                                        .max_h(px(68.))
                                        .overflow_y_scroll()
                                        .child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(cx.theme().muted_foreground)
                                                .child("Use open tab:"),
                                        )
                                        .children(tabs_for_side.into_iter().map(
                                            |(doc_id, label, _)| {
                                                let c_tab = c_body.clone();
                                                Button::new((
                                                    "join-open-tab",
                                                    side * 10_000 + doc_id.raw() as usize,
                                                ))
                                                .label(label)
                                                .ghost()
                                                .xsmall()
                                                .disabled(busy)
                                                .on_click(move |_, _, cx| {
                                                    c_tab.update(cx, |c, cx| {
                                                        let _ = c.join_set_open_tab_side(
                                                            side, doc_id, cx,
                                                        );
                                                    });
                                                })
                                            },
                                        )),
                                )
                            })
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child({
                                        let c_hdr = c_body.clone();
                                        Checkbox::new(("join-has-header", side))
                                            .label("First row is header")
                                            .checked(side_has_header[side])
                                            .disabled(busy || side_is_open_tab[side])
                                            .on_click(move |checked, _, cx| {
                                                c_hdr.update(cx, |c, cx| {
                                                    let _ = c.join_set_side_has_header(
                                                        side, *checked, cx,
                                                    );
                                                });
                                            })
                                    })
                                    .child(
                                        Checkbox::new(("join-unique", side))
                                            .label("Require unique keys on this input")
                                            .checked(req_unique[side])
                                            .disabled(busy)
                                            .on_click(move |checked, _, cx| {
                                                c_unique.update(cx, |c, cx| {
                                                    c.join_set_require_unique(side, *checked, cx);
                                                });
                                            }),
                                    ),
                            )
                    }))
                    // Join mode selector & case sensitivity
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().font_weight(FontWeight::MEDIUM).child("Join mode:"))
                            .children(JOIN_MODES.into_iter().enumerate().map(|(ix, (m, label))| {
                                let c_mode = c_body.clone();
                                let btn = Button::new(("join-mode", ix))
                                    .label(label)
                                    .small()
                                    .disabled(busy)
                                    .on_click(move |_, _, cx| {
                                        c_mode.update(cx, |c, cx| c.join_set_mode(m, cx));
                                    });
                                if m == mode { btn.primary() } else { btn.ghost() }
                            }))
                            .child(div().flex_1())
                            .child({
                                let c_case = c_body.clone();
                                Checkbox::new("join-ignore-case")
                                    .label("Ignore ASCII case")
                                    .checked(ignore_case)
                                    .disabled(busy)
                                    .on_click(move |checked, _, cx| {
                                        c_case.update(cx, |c, cx| {
                                            c.join_set_ignore_ascii_case(*checked, cx);
                                        });
                                    })
                            }),
                    )
                    // Composite key pairs
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child("Matching key columns (1–16)"),
                                    )
                                    .child({
                                        let c_add_key = c_body.clone();
                                        Button::new("join-add-key")
                                            .label("+ Add key pair")
                                            .ghost()
                                            .xsmall()
                                            .disabled(busy || keys.len() >= 16)
                                            .on_click(move |_, _, cx| {
                                                c_add_key.update(cx, |c, cx| {
                                                    c.join_add_key_pair(cx);
                                                });
                                            })
                                    }),
                            )
                            .children(keys.iter().enumerate().map(|(pair_ix, pair)| {
                                let main_cols = side_col_names[0].clone();
                                let add_cols = side_col_names[1].clone();
                                let main_name = main_cols
                                    .get(pair[0])
                                    .cloned()
                                    .unwrap_or_else(|| format!("Column {}", pair[0] + 1));
                                let add_name = add_cols
                                    .get(pair[1])
                                    .cloned()
                                    .unwrap_or_else(|| format!("Column {}", pair[1] + 1));
                                let main_len = main_cols.len().max(1);
                                let add_len = add_cols.len().max(1);
                                let c_m = c_body.clone();
                                let c_a = c_body.clone();
                                let c_rm = c_body.clone();
                                let next_main = (pair[0] + 1) % main_len;
                                let next_add = (pair[1] + 1) % add_len;
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new(("join-key-main", pair_ix))
                                            .label(format!("Main: {}: {main_name}", pair[0] + 1))
                                            .small()
                                            .disabled(busy || main_cols.is_empty())
                                            .on_click(move |_, _, cx| {
                                                c_m.update(cx, |c, cx| {
                                                    c.join_set_key_column(pair_ix, 0, next_main, cx);
                                                });
                                            }),
                                    )
                                    .child(div().child("="))
                                    .child(
                                        Button::new(("join-key-add", pair_ix))
                                            .label(format!("Add: {}: {add_name}", pair[1] + 1))
                                            .small()
                                            .disabled(busy || add_cols.is_empty())
                                            .on_click(move |_, _, cx| {
                                                c_a.update(cx, |c, cx| {
                                                    c.join_set_key_column(pair_ix, 1, next_add, cx);
                                                });
                                            }),
                                    )
                                    .when(keys.len() > 1, |row| {
                                        row.child(
                                            Button::new(("join-key-rm", pair_ix))
                                                .label("×")
                                                .ghost()
                                                .xsmall()
                                                .disabled(busy)
                                                .on_click(move |_, _, cx| {
                                                    c_rm.update(cx, |c, cx| {
                                                        c.join_remove_key_pair(pair_ix, cx);
                                                    });
                                                }),
                                        )
                                    })
                            })),
                    )
                    // Output columns
                    .when(!out_cols.is_empty(), |el| {
                        let total = out_cols.len();
                        el.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(format!("Output columns ({total})")),
                                )
                                .child(
                                    div()
                                        .id("join-output-cols-list")
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .max_h(px(120.))
                                        .overflow_y_scroll()
                                        .border_1()
                                        .border_color(cx.theme().border)
                                        .rounded(px(4.))
                                        .p_1p5()
                                        .children(out_cols.iter().enumerate().map(|(ix, col)| {
                                            let c_t = c_body.clone();
                                            let c_u = c_body.clone();
                                            let c_d = c_body.clone();
                                            let src_tag = match col.source {
                                                JoinSource::Main => "Main",
                                                JoinSource::Additional => "Add",
                                            };
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1()
                                                .child(
                                                    Checkbox::new(("join-out-col", ix))
                                                        .label(format!("[{src_tag}] {}", col.name))
                                                        .checked(col.included)
                                                        .disabled(busy)
                                                        .on_click(move |checked, _, cx| {
                                                            c_t.update(cx, |c, cx| {
                                                                c.join_toggle_output_column(
                                                                    ix, *checked, cx,
                                                                );
                                                            });
                                                        }),
                                                )
                                                .child(div().flex_1())
                                                .child(
                                                    Button::new(("join-col-up", ix))
                                                        .label("↑")
                                                        .ghost()
                                                        .xsmall()
                                                        .disabled(busy || ix == 0)
                                                        .on_click(move |_, _, cx| {
                                                            c_u.update(cx, |c, cx| {
                                                                c.join_move_output_column(
                                                                    ix,
                                                                    ix - 1,
                                                                    cx,
                                                                );
                                                            });
                                                        }),
                                                )
                                                .child(
                                                    Button::new(("join-col-dn", ix))
                                                        .label("↓")
                                                        .ghost()
                                                        .xsmall()
                                                        .disabled(busy || ix + 1 >= total)
                                                        .on_click(move |_, _, cx| {
                                                            c_d.update(cx, |c, cx| {
                                                                c.join_move_output_column(
                                                                    ix,
                                                                    ix + 1,
                                                                    cx,
                                                                );
                                                            });
                                                        }),
                                                )
                                        })),
                                ),
                        )
                    })
                    .when_some(progress_text, |el, text| {
                        el.child(
                            div()
                                .id("join-progress")
                                .role(Role::Status)
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(text),
                        )
                    })
                    .when_some(plan_stats, |el, (summary, preview_rows)| {
                        el.child(
                            div()
                                .id("join-plan-summary")
                                .flex()
                                .flex_col()
                                .gap_1()
                                .p_2()
                                .border_1()
                                .border_color(cx.theme().border)
                                .rounded(px(4.))
                                .child(div().text_size(px(12.)).child(summary))
                                .children(preview_rows.into_iter().map(|line| {
                                    div()
                                        .text_size(px(11.))
                                        .text_color(cx.theme().muted_foreground)
                                        .truncate()
                                        .child(line)
                                })),
                        )
                    })
                    .when(needs_expansion, |el| {
                        let c_exp = c_body.clone();
                        el.child(
                            Checkbox::new("join-ack-expansion")
                                .label("I understand many-to-many key matches expand output rows")
                                .checked(ack_expansion)
                                .disabled(busy)
                                .on_click(move |checked, _, cx| {
                                    c_exp.update(cx, |c, cx| {
                                        c.join_set_acknowledge_expansion(*checked, cx);
                                    });
                                }),
                        )
                    })
                    .when(needs_layout, |el| {
                        let c_lay = c_body.clone();
                        el.child(
                            Checkbox::new("join-ack-layout")
                                .label("I reviewed the additional columns detected beyond initial headers")
                                .checked(ack_layout)
                                .disabled(busy)
                                .on_click(move |checked, _, cx| {
                                    c_lay.update(cx, |c, cx| {
                                        c.join_set_acknowledge_layout(*checked, cx);
                                    });
                                }),
                        )
                    })
                    .when_some(dest_label, |el, label| {
                        el.child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(label),
                        )
                    })
                    .when_some(result_text, |el, text| {
                        el.child(
                            div()
                                .id("join-result")
                                .role(Role::Status)
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(text),
                        )
                    })
                    .when_some(message, |el, msg| {
                        el.child(
                            div()
                                .id("join-message")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(msg),
                        )
                    });
                content.child(body)
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .when(busy, |f| {
                        f.child(
                            Button::new("join-cancel-job")
                                .label("Cancel Job")
                                .danger()
                                .small()
                                .on_click(move |_, _, cx| {
                                    c_cancel.update(cx, |c, cx| c.join_cancel(cx));
                                }),
                        )
                    })
                    .child(
                        Button::new("join-reset")
                            .label("Reset")
                            .ghost()
                            .small()
                            .disabled(busy)
                            .on_click(move |_, _, cx| {
                                c_reset.update(cx, |c, cx| {
                                    c.join_clear_side(0, cx);
                                    c.join_clear_side(1, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("join-close")
                            .label("Close")
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DialogCancel), cx);
                            }),
                    )
                    .child(
                        Button::new("join-preview")
                            .label("Preview")
                            .small()
                            .disabled(!ready_to_analyze)
                            .on_click(move |_, _, cx| {
                                c_preview.update(cx, |c, cx| c.join_start_analysis(cx));
                            }),
                    )
                    .child(
                        Button::new("join-choose-dest")
                            .label("Choose Output…")
                            .small()
                            .disabled(busy || !has_plan)
                            .on_click(move |_, _, cx| {
                                let default_dir = std::env::current_dir()
                                    .unwrap_or_else(|_| PathBuf::from("."));
                                let sel = cx.prompt_for_new_path(&default_dir, Some("joined.csv"));
                                let c_dest = c_dest.clone();
                                cx.spawn(async move |cx| {
                                    if let Ok(Ok(Some(path))) = sel.await {
                                        cx.update(|cx| {
                                            c_dest.update(cx, |c, cx| {
                                                let _ = c.join_prepare_destination(path, cx);
                                            });
                                        });
                                    }
                                })
                                .detach();
                            }),
                    )
                    .child(
                        Button::new("join-run")
                            .label("Create Joined File")
                            .primary()
                            .small()
                            .disabled(!can_write)
                            .on_click(move |_, _, cx| {
                                c_write.update(cx, |c, cx| {
                                    let _ = c.join_start_write(cx);
                                });
                            }),
                    )
                    .when_some(finished_path, |f, out_path| {
                        let open_output = open_output.clone();
                        f.child(
                            Button::new("join-open-result")
                                .label("Open Joined File")
                                .primary()
                                .small()
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    open_output(out_path.clone(), window, cx);
                                }),
                        )
                    }),
            )
            .on_close(move |_, window, cx| {
                // Always release open-tab DocumentLeases when Join Files closes.
                c_close.update(cx, |c, cx| c.join_release_all(cx));
                on_closed(window, cx);
            })
    });
}

// =============================================================================
// 3. Inspect File & Recovery Dialog
// =============================================================================

struct InspectDialogState {
    controller: Entity<WorkbenchController>,
    delimiter: Entity<ChoiceState>,
    header_mode: Entity<ChoiceState>,
    encoding: Entity<ChoiceState>,
    _subs: Vec<Subscription>,
}

pub fn open_inspect_dialog(
    controller: Entity<WorkbenchController>,
    window_id: WindowId,
    on_open_recovered: impl Fn(PathBuf, &mut Window, &mut App) + 'static,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let (init_delim, init_hdr, init_enc) = {
        let wf = controller.read(cx).inspect();
        (
            delimiter_to_index(wf.delimiter),
            header_mode_to_index(wf.header_mode),
            encoding_to_index(wf.encoding),
        )
    };
    let delimiter = make_select(DELIMITER_LABELS.map(str::to_owned), init_delim, window, cx);
    let header_mode = make_select(HEADER_MODE_LABELS.map(str::to_owned), init_hdr, window, cx);
    let encoding = make_select(ENCODING_LABELS.map(str::to_owned), init_enc, window, cx);

    let state = cx.new(|cx| {
        let c1 = controller.clone();
        let c2 = controller.clone();
        let c3 = controller.clone();
        let d_ref = delimiter.clone();
        let h_ref = header_mode.clone();
        let e_ref = encoding.clone();
        let sync_opts = move |ctrl: &Entity<WorkbenchController>,
                              d_ent: &Entity<ChoiceState>,
                              h_ent: &Entity<ChoiceState>,
                              e_ent: &Entity<ChoiceState>,
                              cx: &mut App| {
            let d = delimiter_from_index(*d_ent.read(cx).selected_value().unwrap_or(&0));
            let h = header_mode_from_index(*h_ent.read(cx).selected_value().unwrap_or(&0));
            let e = encoding_from_label(*e_ent.read(cx).selected_value().unwrap_or(&0));
            ctrl.update(cx, |c, cx| c.inspect_set_options(d, h, e, cx));
        };
        let subs = vec![
            cx.observe(&controller, |_, _, cx| cx.notify()),
            {
                let h = h_ref.clone();
                let e = e_ref.clone();
                cx.subscribe_in(
                    &delimiter,
                    window,
                    move |_, d, _: &SelectEvent<SearchableVec<Choice>>, _, cx| {
                        sync_opts(&c1, d, &h, &e, cx);
                    },
                )
            },
            {
                let d = d_ref.clone();
                let e = e_ref.clone();
                cx.subscribe_in(
                    &header_mode,
                    window,
                    move |_, h, _: &SelectEvent<SearchableVec<Choice>>, _, cx| {
                        sync_opts(&c2, &d, h, &e, cx);
                    },
                )
            },
            {
                let d = d_ref.clone();
                let h = h_ref.clone();
                cx.subscribe_in(
                    &encoding,
                    window,
                    move |_, e, _: &SelectEvent<SearchableVec<Choice>>, _, cx| {
                        sync_opts(&c3, &d, &h, e, cx);
                    },
                )
            },
        ];
        InspectDialogState {
            controller: controller.clone(),
            delimiter: delimiter.clone(),
            header_mode: header_mode.clone(),
            encoding: encoding.clone(),
            _subs: subs,
        }
    });

    let open_recovered_cb = std::rc::Rc::new(on_open_recovered);
    let on_closed = std::rc::Rc::new(on_closed);
    window.open_dialog(cx, move |dialog, _, cx| {
        let on_closed = on_closed.clone();
        let state_body = state.clone();
        let c_footer = controller.clone();
        let open_recovered = open_recovered_cb.clone();

        let (busy, has_path, has_selected, recovered_path, suggested_dir) = {
            let wf = controller.read(cx).inspect();
            let dir = wf
                .display_path
                .as_ref()
                .and_then(|p| p.parent())
                .unwrap_or_else(|| std::path::Path::new("."))
                .to_path_buf();
            (
                wf.busy(),
                wf.path.is_some(),
                !wf.selected.is_empty() && wf.report.is_some(),
                wf.recovery_result.as_ref().map(|r| r.destination.clone()),
                dir,
            )
        };
        let c_inspect = c_footer.clone();
        let c_recover = c_footer.clone();
        let c_cancel = c_footer.clone();

        dialog
            .title("Inspect File & Recovery")
            .w(px(700.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                let st = state_body.read(cx);
                let ctrl = st.controller.clone();
                let wf = ctrl.read(cx).inspect();
                let busy = wf.busy();
                let target_label = wf
                    .display_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "No file selected".into());
                let findings: Vec<(usize, String, bool, bool)> = wf
                    .report
                    .as_ref()
                    .map(|r| {
                        r.findings
                            .iter()
                            .enumerate()
                            .map(|(ix, d)| {
                                (ix, d.details(), r.can_omit(ix), wf.selected.contains(&ix))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let report_summary = wf.report.as_ref().map(|r| {
                    format!(
                        "Scanned {} records · {} finding{} ({} selected for omission)",
                        r.records,
                        r.total_findings,
                        if r.total_findings == 1 { "" } else { "s" },
                        wf.selected.len()
                    )
                });
                let preview_text = wf.preview.as_ref().map(|p| p.text.clone());
                let can_jump =
                    wf.target_doc_id.is_some() && wf.finding_jump_target(wf.focused).is_some();
                let focused_ix = wf.focused;
                let progress_text = wf.progress().map(|p| {
                    format!(
                        "Scanning {} / {}…",
                        format_bytes(p.bytes_scanned),
                        format_bytes(p.total_bytes)
                    )
                });
                let recovery_text = wf.recovery_result.as_ref().map(|r| {
                    format!(
                        "Wrote recovery copy ({} rows retained, {} omitted) to {}",
                        r.retained_records,
                        r.removed_records,
                        r.destination.display()
                    )
                });
                let message = wf.message.clone();

                let c_browse = ctrl.clone();
                let c_all = ctrl.clone();
                let c_jump = ctrl.clone();
                let delim_ent = st.delimiter.clone();
                let hdr_ent = st.header_mode.clone();
                let enc_ent = st.encoding.clone();

                let body = div()
                    .id("inspect-dialog-scroll")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_size(px(12.))
                                    .child(target_label),
                            )
                            .child(
                                Button::new("inspect-browse")
                                    .label("Choose File…")
                                    .small()
                                    .disabled(busy)
                                    .on_click({
                                        let delim_reset = delim_ent.clone();
                                        let hdr_reset = hdr_ent.clone();
                                        let enc_reset = enc_ent.clone();
                                        move |_, window, cx| {
                                            let sel = cx.prompt_for_paths(PathPromptOptions {
                                                files: true,
                                                directories: false,
                                                multiple: false,
                                                prompt: Some("Select file to inspect".into()),
                                            });
                                            let c_browse = c_browse.clone();
                                            let wh = window.window_handle();
                                            let delim_reset = delim_reset.clone();
                                            let hdr_reset = hdr_reset.clone();
                                            let enc_reset = enc_reset.clone();
                                            cx.spawn(async move |cx| {
                                                if let Ok(Ok(Some(paths))) = sel.await
                                                    && let Some(path) = paths.into_iter().next()
                                                {
                                                    let _ = wh.update(cx, |_, window, cx| {
                                                        c_browse.update(cx, |c, cx| {
                                                            c.inspect_configure_path(path, cx);
                                                        });
                                                        delim_reset.update(cx, |s, cx| {
                                                            s.set_selected_value(
                                                                &0usize, window, cx,
                                                            );
                                                        });
                                                        hdr_reset.update(cx, |s, cx| {
                                                            s.set_selected_value(
                                                                &0usize, window, cx,
                                                            );
                                                        });
                                                        enc_reset.update(cx, |s, cx| {
                                                            s.set_selected_value(
                                                                &0usize, window, cx,
                                                            );
                                                        });
                                                    });
                                                }
                                            })
                                            .detach();
                                        }
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(140.))
                                    .child(Select::new(&delim_ent).small().disabled(busy)),
                            )
                            .child(
                                div()
                                    .w(px(180.))
                                    .child(Select::new(&hdr_ent).small().disabled(busy)),
                            )
                            .child(
                                div()
                                    .w(px(150.))
                                    .child(Select::new(&enc_ent).small().disabled(busy)),
                            ),
                    )
                    .when_some(progress_text, |el, text| {
                        el.child(
                            div()
                                .id("inspect-progress")
                                .role(Role::Status)
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(text),
                        )
                    })
                    .when_some(report_summary, |el, summary| {
                        el.child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .id("inspect-summary")
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(summary),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_1()
                                        .when(can_jump, |btns| {
                                            btns.child(
                                                Button::new("inspect-jump")
                                                    .label("Jump to Row")
                                                    .ghost()
                                                    .xsmall()
                                                    .on_click(move |_, window, cx| {
                                                        c_jump.update(cx, |c, cx| {
                                                            c.inspect_jump_to_finding(
                                                                window_id, focused_ix, cx,
                                                            );
                                                        });
                                                        window.close_dialog(cx);
                                                    }),
                                            )
                                        })
                                        .child(
                                            Button::new("inspect-select-all")
                                                .label("Select All Eligible")
                                                .ghost()
                                                .xsmall()
                                                .disabled(busy || findings.is_empty())
                                                .on_click(move |_, _, cx| {
                                                    c_all.update(cx, |c, cx| {
                                                        c.inspect_select_all_eligible(cx);
                                                    });
                                                }),
                                        ),
                                ),
                        )
                    })
                    .when(!findings.is_empty(), |el| {
                        el.child(
                            div()
                                .id("inspect-findings-list")
                                .flex()
                                .flex_col()
                                .gap_1()
                                .max_h(px(150.))
                                .overflow_y_scroll()
                                .border_1()
                                .border_color(cx.theme().border)
                                .rounded(px(4.))
                                .p_1p5()
                                .children(findings.into_iter().map(
                                    |(ix, detail, can_omit, checked)| {
                                        let c_tog = ctrl.clone();
                                        let c_foc = ctrl.clone();
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .when(can_omit, |row| {
                                                row.child(
                                                    Checkbox::new(("inspect-omit", ix))
                                                        .checked(checked)
                                                        .disabled(busy)
                                                        .on_click(move |_, _, cx| {
                                                            c_tog.update(cx, |c, cx| {
                                                                c.inspect_toggle_finding(ix, cx);
                                                            });
                                                        }),
                                                )
                                            })
                                            .child(
                                                Button::new(("inspect-focus", ix))
                                                    .label(detail)
                                                    .ghost()
                                                    .xsmall()
                                                    .on_click(move |_, _, cx| {
                                                        c_foc.update(cx, |c, cx| {
                                                            c.inspect_focus_finding(ix, cx);
                                                        });
                                                    }),
                                            )
                                    },
                                )),
                        )
                    })
                    .when_some(preview_text, |el, text| {
                        el.child(
                            div()
                                .id("inspect-source-preview")
                                .p_2()
                                .rounded(px(4.))
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().secondary)
                                .text_size(px(11.))
                                .child(text),
                        )
                    })
                    .when_some(recovery_text, |el, text| {
                        el.child(
                            div()
                                .id("inspect-recovery-result")
                                .role(Role::Status)
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(text),
                        )
                    })
                    .when_some(message, |el, msg| {
                        el.child(
                            div()
                                .id("inspect-message")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(msg),
                        )
                    });
                content.child(body)
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .when(busy, |f| {
                        f.child(
                            Button::new("inspect-cancel-job")
                                .label("Cancel Job")
                                .danger()
                                .small()
                                .on_click(move |_, _, cx| {
                                    c_cancel.update(cx, |c, cx| c.inspect_cancel(cx));
                                }),
                        )
                    })
                    .child(
                        Button::new("inspect-close")
                            .label("Close")
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DialogCancel), cx);
                            }),
                    )
                    .child(
                        Button::new("inspect-run")
                            .label("Inspect")
                            .primary()
                            .small()
                            .disabled(busy || !has_path)
                            .on_click(move |_, _, cx| {
                                c_inspect.update(cx, |c, cx| c.inspect_start(cx));
                            }),
                    )
                    .child(
                        Button::new("inspect-recover")
                            .label("Create Recovery Copy…")
                            .small()
                            .disabled(busy || !has_selected)
                            .on_click(move |_, _, cx| {
                                let sel =
                                    cx.prompt_for_new_path(&suggested_dir, Some("recovered.csv"));
                                let c_recover = c_recover.clone();
                                cx.spawn(async move |cx| {
                                    if let Ok(Ok(Some(path))) = sel.await {
                                        cx.update(|cx| {
                                            c_recover.update(cx, |c, cx| {
                                                let _ = c.inspect_start_recovery(path, cx);
                                            });
                                        });
                                    }
                                })
                                .detach();
                            }),
                    )
                    .when_some(recovered_path, |f, out_path| {
                        let open_recovered = open_recovered.clone();
                        f.child(
                            Button::new("inspect-open-recovered")
                                .label("Open Recovery Copy")
                                .primary()
                                .small()
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    open_recovered(out_path.clone(), window, cx);
                                }),
                        )
                    }),
            )
            .on_close(move |_, window, cx| {
                on_closed(window, cx);
            })
    });
}

// =============================================================================
// 4. Temporary Storage & Index Cache Dialog
// =============================================================================

pub fn open_temporary_storage_dialog(
    controller: Entity<WorkbenchController>,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    controller.update(cx, |c, cx| c.start_storage_inspection(cx));
    let observer = cx.new(|cx| {
        let sub = cx.observe(&controller, |_, _, cx| cx.notify());
        (controller.clone(), sub)
    });
    let action_error = cx.new(|_| Option::<String>::None);
    let on_closed = std::rc::Rc::new(on_closed);

    window.open_dialog(cx, move |dialog, _, cx| {
        let on_closed = on_closed.clone();
        let _obs = observer.read(cx);
        let c_body = controller.clone();
        let err_state = action_error.clone();

        dialog
            .title("Temporary Storage & Index Cache")
            .w(px(560.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                let c = c_body.read(cx);
                let dir_display = c.working_directory().display().to_string();
                let is_custom = c.custom_working_directory();
                let load_error = c.storage_load_error().map(str::to_owned);
                let checking = c.storage_check_running();
                let check_res = c.storage_check_result().cloned();
                let cache_enabled = c.index_cache_enabled();
                let cache_clearing = c.cache_clear_running();
                let cache_status = c.cache_clear_status().cloned();
                let local_err = err_state.read(cx).clone();

                let c_choose = c_body.clone();
                let c_reset = c_body.clone();
                let c_cache_tog = c_body.clone();
                let c_cache_clr = c_body.clone();
                let err_choose = err_state.clone();
                let err_reset = err_state.clone();
                let err_cache = err_state.clone();

                let space_line = if checking {
                    "Checking temporary folder space…".to_owned()
                } else {
                    match check_res {
                        Some(Ok((space, retained))) => format!(
                            "Free space: {} · Retained working files: {}",
                            format_bytes(space.available_bytes),
                            format_bytes(retained)
                        ),
                        Some(Err(err)) => format!("Storage warning: {err}"),
                        None => "Storage check not run yet.".to_owned(),
                    }
                };

                let body = div()
                    .id("storage-dialog-scroll")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .max_h(px(380.))
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(if is_custom {
                                        "Temporary folder (Custom)"
                                    } else {
                                        "Temporary folder (System Default)"
                                    }),
                            )
                            .child(
                                div()
                                    .id("storage-folder-path")
                                    .text_size(px(12.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(dir_display),
                            )
                            .child(
                                div()
                                    .id("storage-space-readout")
                                    .text_size(px(12.))
                                    .child(space_line),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .mt_1()
                                    .child(
                                        Button::new("storage-choose-folder")
                                            .label("Choose Folder…")
                                            .small()
                                            .on_click(move |_, _, cx| {
                                                let sel = cx.prompt_for_paths(PathPromptOptions {
                                                    files: false,
                                                    directories: true,
                                                    multiple: false,
                                                    prompt: Some("Choose temporary folder".into()),
                                                });
                                                let c_choose = c_choose.clone();
                                                let err_choose = err_choose.clone();
                                                cx.spawn(async move |cx| {
                                                    if let Ok(Ok(Some(paths))) = sel.await
                                                        && let Some(dir) = paths.into_iter().next()
                                                    {
                                                        cx.update(|cx| {
                                                            let res = c_choose.update(cx, |c, cx| {
                                                                c.set_scratch_directory(dir, cx)
                                                            });
                                                            err_choose.update(cx, |e, cx| {
                                                                *e = res.err();
                                                                cx.notify();
                                                            });
                                                        });
                                                    }
                                                })
                                                .detach();
                                            }),
                                    )
                                    .child(
                                        Button::new("storage-reset-default")
                                            .label("Reset to Default")
                                            .ghost()
                                            .small()
                                            .disabled(!is_custom)
                                            .on_click(move |_, _, cx| {
                                                let res = c_reset.update(cx, |c, cx| {
                                                    c.reset_scratch_directory(cx)
                                                });
                                                err_reset.update(cx, |e, cx| {
                                                    *e = res.err();
                                                    cx.notify();
                                                });
                                            }),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .p_2()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(cx.theme().border)
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "Adaptive out-of-core memory budget: {} per background sort, join, or duplicate scan. Temporary working copies are removed automatically when tabs close.",
                                format_bytes(ADAPTIVE_MEMORY_BUDGET_BYTES)
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                Checkbox::new("storage-index-cache-toggle")
                                    .label("Enable persistent sidecar index cache for faster reopens")
                                    .checked(cache_enabled)
                                    .on_click(move |checked, _, cx| {
                                        let res = c_cache_tog.update(cx, |c, cx| {
                                            c.set_index_cache_enabled(*checked, cx)
                                        });
                                        err_cache.update(cx, |e, cx| {
                                            *e = res.err();
                                            cx.notify();
                                        });
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new("storage-clear-cache")
                                            .label(if cache_clearing {
                                                "Clearing Cache…"
                                            } else {
                                                "Clear Index Cache"
                                            })
                                            .small()
                                            .disabled(cache_clearing)
                                            .on_click(move |_, _, cx| {
                                                c_cache_clr.update(cx, |c, cx| {
                                                    c.start_clear_index_cache(cx);
                                                });
                                            }),
                                    )
                                    .when_some(cache_status, |row, res| {
                                        let (text, is_err) = match res {
                                            Ok(s) => (s, false),
                                            Err(e) => (e, true),
                                        };
                                        row.child(
                                            div()
                                                .id("storage-cache-status")
                                                .text_size(px(12.))
                                                .text_color(if is_err {
                                                    cx.theme().danger
                                                } else {
                                                    cx.theme().muted_foreground
                                                })
                                                .child(text),
                                        )
                                    }),
                            ),
                    )
                    .when_some(load_error.or(local_err), |el, msg| {
                        el.child(
                            div()
                                .id("storage-error")
                                .role(Role::Alert)
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(msg),
                        )
                    });
                content.child(body)
            })
            .footer(
                div().flex().justify_end().child(
                    Button::new("storage-close")
                        .label("Done")
                        .primary()
                        .small()
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(DialogCancel), cx);
                        }),
                ),
            )
            .on_close(move |_, window, cx| {
                on_closed(window, cx);
            })
    });
}

// =============================================================================
// 5. About Quarry Dialog
// =============================================================================

#[cfg(target_os = "macos")]
const QUARRY_BUNDLE_ID: &str = "io.github.danchamorro.quarry";
#[cfg(target_os = "macos")]
const MAX_INFO_PLIST_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BundleBuildInfo {
    pub version: Option<String>,
    pub build: Option<String>,
    pub architecture: Option<String>,
    pub revision: Option<String>,
    pub source_status: Option<String>,
}

#[cfg(any(target_os = "macos", test))]
fn unescape_xml_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

#[cfg(any(target_os = "macos", test))]
fn plist_string_value(xml: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let (_, after_key) = xml.split_once(&needle)?;
    let trimmed = after_key.trim_start();
    let rest = trimmed.strip_prefix("<string>")?;
    let (value, _) = rest.split_once("</string>")?;
    let decoded = unescape_xml_entities(value.trim());
    (!decoded.is_empty()).then_some(decoded)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_info_plist(xml: &str, expected_bundle_id: &str) -> Option<BundleBuildInfo> {
    if plist_string_value(xml, "CFBundleIdentifier").as_deref() != Some(expected_bundle_id) {
        return None;
    }
    Some(BundleBuildInfo {
        version: plist_string_value(xml, "CFBundleShortVersionString"),
        build: plist_string_value(xml, "CFBundleVersion"),
        architecture: plist_string_value(xml, "QuarryArchitecture"),
        revision: plist_string_value(xml, "QuarryGitRevision"),
        source_status: plist_string_value(xml, "QuarrySourceStatus"),
    })
}

fn read_bundle_build_info() -> BundleBuildInfo {
    #[cfg(target_os = "macos")]
    {
        use std::io::Read;

        let Some(plist_path) = std::env::current_exe().ok().and_then(|exe| {
            exe.parent()
                .and_then(|p| p.parent())
                .map(|p| p.join("Info.plist"))
        }) else {
            return BundleBuildInfo::default();
        };
        let Ok(file) = quarry_core::open_regular_file_nofollow(&plist_path) else {
            return BundleBuildInfo::default();
        };
        let mut xml = String::new();
        if file
            .take(MAX_INFO_PLIST_BYTES.saturating_add(1))
            .read_to_string(&mut xml)
            .is_err()
            || xml.len() as u64 > MAX_INFO_PLIST_BYTES
        {
            return BundleBuildInfo::default();
        }
        parse_info_plist(&xml, QUARRY_BUNDLE_ID).unwrap_or_default()
    }
    #[cfg(not(target_os = "macos"))]
    {
        BundleBuildInfo::default()
    }
}

pub(crate) fn format_build_details(info: &BundleBuildInfo) -> String {
    let version = info.version.as_deref().unwrap_or(env!("CARGO_PKG_VERSION"));
    let arch = info
        .architecture
        .as_deref()
        .unwrap_or(std::env::consts::ARCH);
    format!(
        "Quarry {version}\nBuild: {}\nRevision: {}\nSource status: {}\nPlatform: {} / {}\nOffline-only local desktop workbench — zero telemetry or network requests.",
        info.build.as_deref().unwrap_or("source build"),
        info.revision.as_deref().unwrap_or("not recorded"),
        info.source_status.as_deref().unwrap_or("not recorded"),
        std::env::consts::OS,
        arch,
    )
}

pub fn open_about_dialog(
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let copied = cx.new(|_| false);
    let build_info = read_bundle_build_info();
    let version = build_info
        .version
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let details = format_build_details(&build_info);
    let on_closed = std::rc::Rc::new(on_closed);

    window.open_dialog(cx, move |dialog, _, cx| {
        let on_closed = on_closed.clone();
        let details_copy = details.clone();
        let version_copy = version.clone();
        let copied_state = copied.clone();
        let copied_now = *copied.read(cx);

        dialog
            .title("About Quarry")
            .w(px(460.))
            .overlay_closable(true)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                content.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .id("about-title")
                                .text_size(px(18.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("Quarry {version_copy}")),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child("Fast, offline, privacy-first desktop workbench for massive delimited files."),
                        )
                        .child(
                            div()
                                .p_2()
                                .rounded(px(4.))
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().secondary)
                                .text_size(px(11.))
                                .child(details_copy.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(cx.theme().muted_foreground)
                                .child("User Guide & Issues: https://github.com/danchamorro/quarry"),
                        ),
                )
            })
            .footer(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child({
                        let details_btn = details.clone();
                        Button::new("about-copy-details")
                            .label(if copied_now {
                                "Copied Build Details"
                            } else {
                                "Copy Build Details"
                            })
                            .small()
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    details_btn.clone(),
                                ));
                                copied_state.update(cx, |flag, cx| {
                                    *flag = true;
                                    cx.notify();
                                });
                            })
                    })
                    .child(
                        Button::new("about-close")
                            .label("Close")
                            .primary()
                            .small()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DialogCancel), cx);
                            }),
                    ),
            )
            .on_close(move |_, window, cx| {
                on_closed(window, cx);
            })
    });
}
