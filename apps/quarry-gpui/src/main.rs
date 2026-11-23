mod document;
mod grid;
mod panels;

#[cfg(test)]
mod ui_tests;

use std::{path::PathBuf, time::Duration};

use gpui_kit::{
    component::{
        ActiveTheme, Disableable, Root, Sizable, Theme, ThemeMode, WindowExt,
        button::{Button, ButtonVariants},
        dialog::{Cancel as DialogCancel, Confirm as DialogConfirm},
        input::{Input, InputEvent, InputState, Textarea, TextareaState},
        table::{DataTable, TableEvent, TableState},
    },
    prelude::*,
    *,
};
use quarry_core::OpenOptions;

use document::DocumentInfo;
use grid::{Bridge, Command, Grid, Response};
use panels::{PanelEvent, Panels};

gpui_kit::actions!(
    quarry_comparison,
    [OpenFile, SaveCopy, Quit, EditCell, UndoEdit, RedoEdit]
);

struct Quarry {
    focus_handle: FocusHandle,
    bridge: Bridge,
    table: Entity<TableState<Grid>>,
    panels: Entity<Panels>,
    row_input: Entity<InputState>,
    info: Option<DocumentInfo>,
    busy: bool,
    confirming: bool,
    editing: bool,
    editor_status: Option<Entity<CellEditStatus>>,
    message: Option<(String, bool)>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
struct CellEditStatus {
    pending: bool,
    error: Option<String>,
}

impl Quarry {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let bridge = Bridge::new();
        let table = cx.new(|cx| {
            TableState::new(Grid::new(bridge.requester()), window, cx)
                .cell_selectable(true)
                .row_header(false)
                .col_movable(false)
        });
        let panels = cx.new(|cx| Panels::new(window, cx));
        let row_input = cx.new(|cx| InputState::new(window, cx).default_value("1"));
        let subscriptions = vec![
            cx.subscribe_in(&table, window, |this, _, event, window, cx| match event {
                TableEvent::DoubleClickedCell(row, column) => this.edit_cell(*row, *column, cx),
                TableEvent::ColumnWidthsChanged(widths) => {
                    this.table.update(cx, |table, _| table.delegate_mut().set_widths(widths));
                }
                _ => { let _ = window; }
            }),
            cx.subscribe_in(&panels, window, |this, _, event, window, cx| {
                match event {
                    PanelEvent::Columns(columns) => this.table.update(cx, |table, cx| {
                        table.delegate_mut().set_columns(columns.clone());
                        table.refresh(cx);
                        cx.notify();
                    }),
                    PanelEvent::AutoFit => this.table.update(cx, |table, cx| {
                        table.delegate_mut().auto_fit();
                        table.refresh(cx);
                        cx.notify();
                    }),
                    PanelEvent::Filter(query) => {
                        this.table.update(cx, |table, cx| table.scroll_to_row(0, cx));
                        this.row_input.update(cx, |state, cx| state.set_value("1", window, cx));
                        this.command(Command::Filter(query.clone()), cx);
                    },
                    PanelEvent::ClearFilter => {
                        this.table.update(cx, |table, cx| table.scroll_to_row(0, cx));
                        this.row_input.update(cx, |state, cx| state.set_value("1", window, cx));
                        this.command(Command::ClearFilter, cx);
                    },
                    PanelEvent::Reopen(options) => {
                        if let Some(info) = &this.info {
                            if info.dirty {
                                this.message = Some(("Undo your edits or save a copy and open it before changing the format.".into(), true));
                            } else {
                                this.command(Command::Open(info.path.clone(), *options), cx);
                            }
                        }
                    }
                }
                cx.notify();
            }),
            cx.subscribe_in(&row_input, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) { this.go_to_row(cx); }
            }),
        ];
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |this, cx| {
                this.close_requested(window, cx);
                false
            })
            .unwrap_or(true)
        });
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(32))
                    .await;
                if this
                    .update_in(cx, |this, window, cx| this.poll(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            focus_handle,
            bridge,
            table,
            panels,
            row_input,
            info: None,
            busy: false,
            confirming: false,
            editing: false,
            editor_status: None,
            message: None,
            _subscriptions: subscriptions,
        }
    }

    fn command(&mut self, command: Command, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        match self.bridge.send(command) {
            Ok(()) => {
                self.busy = true;
                self.message = None;
                self.panels
                    .update(cx, |panels, cx| panels.set_busy(true, cx));
            }
            Err(error) => self.message = Some((error, true)),
        }
        cx.notify();
    }

    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let responses = self.bridge.drain();
        if responses.is_empty() {
            return;
        }
        for response in responses {
            match response {
                Response::Opened(info) => {
                    self.panels
                        .update(cx, |panels, cx| panels.finish_reopen(true, cx));
                    self.table.update(cx, |table, cx| {
                        table.delegate_mut().set_info(info.clone());
                        table.delegate_mut().reset_columns();
                        table.refresh(cx);
                        table.scroll_to_row(0, cx);
                        table.scroll_to_col(0, cx);
                        cx.notify();
                    });
                    self.row_input
                        .update(cx, |state, cx| state.set_value("1", window, cx));
                    self.update_info(info, window, cx);
                }
                Response::Info(info) => {
                    let changed = self
                        .info
                        .as_ref()
                        .is_none_or(|old| old.headers != info.headers);
                    self.table.update(cx, |table, cx| {
                        table.delegate_mut().set_info(info.clone());
                        if changed {
                            table.refresh(cx);
                        }
                        cx.notify();
                    });
                    self.update_info(info, window, cx);
                }
                Response::Viewport(viewport) => self.table.update(cx, |table, cx| {
                    table.delegate_mut().set_viewport(viewport);
                    cx.notify();
                }),
                Response::Cell { row, column, value } => {
                    self.show_editor(row, column, value, window, cx)
                }
                Response::Saved(path) => {
                    self.message = Some((format!("Copy saved: {}", path.display()), false));
                }
                Response::Done => {
                    self.busy = false;
                    self.panels
                        .update(cx, |panels, cx| panels.set_busy(false, cx));
                    if self
                        .editor_status
                        .as_ref()
                        .is_some_and(|status| status.read(cx).pending)
                    {
                        self.editor_status = None;
                        self.editing = false;
                        window.close_dialog(cx);
                    }
                }
                Response::Error(error) => {
                    if let Some(status) = &self.editor_status {
                        status.update(cx, |status, cx| {
                            if status.pending {
                                status.pending = false;
                                status.error = Some(error.clone());
                                cx.notify();
                            }
                        });
                    }
                    self.panels
                        .update(cx, |panels, cx| panels.finish_reopen(false, cx));
                    self.message = Some((error, true));
                    self.busy = false;
                    self.panels
                        .update(cx, |panels, cx| panels.set_busy(false, cx));
                }
            }
        }
        cx.notify();
    }

    fn update_info(&mut self, info: DocumentInfo, window: &mut Window, cx: &mut Context<Self>) {
        let filename = info.path.file_name().unwrap_or_default().to_string_lossy();
        window.set_window_title(&format!(
            "{}{} · Quarry GPUI comparison",
            filename,
            if info.dirty { " *" } else { "" }
        ));
        let shown = self.table.read(cx).delegate().columns().to_vec();
        self.panels.update(cx, |panels, cx| {
            panels.set_document(&info, shown, window, cx)
        });
        self.info = Some(info);
    }

    fn choose_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.confirming || self.editing {
            return;
        }
        if self.info.as_ref().is_some_and(|info| info.dirty) {
            self.confirming = true;
            let answer = window.prompt(PromptLevel::Warning, "Open another file?", Some("Your edits are in memory. Save a copy first if you want to keep them. Opening another file discards these edits."), &["Cancel", "Discard edits and open"], cx);
            cx.spawn_in(window, async move |this, cx| {
                let discard = matches!(answer.await, Ok(1));
                let _ = this.update_in(cx, |this, window, cx| {
                    this.confirming = false;
                    if discard {
                        this.open_picker(window, cx);
                    }
                });
            })
            .detach();
        } else {
            self.open_picker(window, cx);
        }
    }

    fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirming = true;
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open in Quarry GPUI".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let selection = selection.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.confirming = false;
                match selection {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.command(Command::Open(path, OpenOptions::default()), cx);
                        }
                    }
                    Ok(Err(error)) => {
                        this.message = Some((error.to_string(), true));
                        cx.notify();
                    }
                    _ => {}
                }
            });
        })
        .detach();
    }

    fn choose_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.confirming || self.editing {
            return;
        }
        let Some(info) = &self.info else {
            return;
        };
        let directory = info
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let name = format!(
            "{}-edited.{}",
            info.path.file_stem().unwrap_or_default().to_string_lossy(),
            info.path
                .extension()
                .unwrap_or_else(|| std::ffi::OsStr::new("csv"))
                .to_string_lossy()
        );
        let selection = cx.prompt_for_new_path(directory, Some(&name));
        self.confirming = true;
        cx.spawn_in(window, async move |this, cx| {
            let selection = selection.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.confirming = false;
                match selection {
                    Ok(Ok(Some(path))) => this.command(Command::SaveCopy(path), cx),
                    Ok(Err(error)) => {
                        this.message = Some((error.to_string(), true));
                        cx.notify();
                    }
                    _ => {}
                }
            });
        })
        .detach();
    }

    fn close_requested(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming {
            return;
        }
        if self.busy {
            self.message = Some((
                "Wait for the current operation to finish before closing.".into(),
                false,
            ));
            cx.notify();
            return;
        }
        if self.editing || self.info.as_ref().is_some_and(|info| info.dirty) {
            self.confirming = true;
            let answer = window.prompt(PromptLevel::Warning, "Close this comparison?", Some("Unsaved edits will be discarded. Your original file stays unchanged. Cancel to save a copy first."), &["Cancel", "Discard edits and close"], cx);
            cx.spawn_in(window, async move |this, cx| {
                let discard = matches!(answer.await, Ok(1));
                let _ = this.update_in(cx, |this, window, _cx| {
                    this.confirming = false;
                    if discard {
                        window.remove_window();
                    }
                });
            })
            .detach();
        } else {
            window.remove_window();
        }
    }

    fn go_to_row(&mut self, cx: &mut Context<Self>) {
        let limit = self.info.as_ref().map_or(0, |info| info.available_rows);
        match self.row_input.read(cx).value().parse::<u64>() {
            Ok(row) if row > 0 && row <= limit => {
                self.table
                    .update(cx, |table, cx| table.scroll_to_row((row - 1) as usize, cx));
                self.message = None;
            }
            _ => self.message = Some((format!("Enter a row from 1 to {limit}."), true)),
        }
        cx.notify();
    }

    fn edit_cell(&mut self, row: usize, column: usize, cx: &mut Context<Self>) {
        if self.editing || self.busy {
            return;
        }
        if let Some((row, column)) = self.table.read(cx).delegate().cell_at(row, column) {
            self.command(Command::Cell(row, column), cx);
        }
    }

    fn show_editor(
        &mut self,
        row: u64,
        column: usize,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = self
            .info
            .as_ref()
            .and_then(|info| info.headers.get(column))
            .cloned()
            .unwrap_or_else(|| format!("Column {}", column + 1));
        let input = cx.new(|cx| TextareaState::new(window, cx).default_value(value));
        self.panels
            .update(cx, |panels, cx| panels.close(window, cx));
        self.editing = true;
        let status = cx.new(|_| CellEditStatus::default());
        self.editor_status = Some(status.clone());
        let focus_input = input.clone();
        let apply = cx.weak_entity();
        let close = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let input_value = input.clone();
            let input_view = input.clone();
            let apply = apply.clone();
            let close = close.clone();
            let pending = status.read(cx).pending;
            let error = status.read(cx).error.clone();
            let apply_status = status.clone();
            let cancel_status = status.clone();
            dialog
                .title(format!("{} · source row {}", title, row + 1))
                .w(px(480.))
                .overlay_closable(false)
                .close_button(!pending)
                .keyboard(!pending)
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("cancel-cell-edit")
                                .label("Cancel")
                                .small()
                                .disabled(pending)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(DialogCancel), cx)
                                }),
                        )
                        .child(
                            Button::new("apply-cell-edit")
                                .label("Apply edit")
                                .primary()
                                .small()
                                .disabled(pending)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(DialogConfirm { secondary: false }),
                                        cx,
                                    )
                                }),
                        ),
                )
                .content(move |content, _, cx| {
                    let too_large = input_view.read(cx).value().len() > document::MAX_EDIT_BYTES;
                    content.child(
                        Textarea::new(&input_view)
                            .aria_label("Cell value")
                            .readonly(pending)
                            .h(px(116.)),
                    )
                    .when(too_large, |content| content.child(
                        div().id("cell-edit-error").test_support()
                            .text_color(cx.theme().danger)
                            .child("Cell values must be 1 MiB or smaller. Shorten the value to apply it.")
                    ))
                    .when_some(error.clone().filter(|_| !too_large), |content, error| content.child(
                        div().id("cell-edit-error").test_support()
                            .text_color(cx.theme().danger).child(error)
                    ))
                })
                .on_ok(move |_, window, cx| {
                    let value = input_value.read(cx).value().to_string();
                    if value.len() > document::MAX_EDIT_BYTES {
                        window.refresh();
                        return false;
                    }
                    let _ = apply.update(cx, |this, cx| {
                        if this.busy || apply_status.read(cx).pending {
                            return;
                        }
                        this.command(Command::Edit(row, column, value), cx);
                        apply_status.update(cx, |status, cx| {
                            status.pending = this.busy;
                            status.error = this.message.as_ref().filter(|(_, error)| *error)
                                .map(|(message, _)| message.clone());
                            cx.notify();
                        });
                    });
                    window.refresh();
                    false
                })
                .on_cancel(move |_, _, cx| !cancel_status.read(cx).pending)
                .on_close(move |_, _, cx| {
                    let _ = close.update(cx, |this, cx| {
                        this.editing = false;
                        this.editor_status = None;
                        cx.notify();
                    });
                })
        });
        focus_input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn open_action(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_open(window, cx);
    }
    fn save_action(&mut self, _: &SaveCopy, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_save(window, cx);
    }
    fn quit_action(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        self.close_requested(window, cx);
    }
    fn edit_action(&mut self, _: &EditCell, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((row, column)) = self.table.read(cx).selected_cell() {
            self.edit_cell(row, column, cx);
        }
    }
    fn undo_action(&mut self, _: &UndoEdit, _: &mut Window, cx: &mut Context<Self>) {
        if !self.editing && self.info.as_ref().is_some_and(|info| info.can_undo) {
            self.command(Command::Undo, cx);
        }
    }
    fn redo_action(&mut self, _: &RedoEdit, _: &mut Window, cx: &mut Context<Self>) {
        if !self.editing && self.info.as_ref().is_some_and(|info| info.can_redo) {
            self.command(Command::Redo, cx);
        }
    }
}

impl Render for Quarry {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.focused(cx).is_none() {
            self.focus_handle.focus(window, cx);
        }
        let loaded = self.info.is_some();
        let filename = self
            .info
            .as_ref()
            .map(|info| {
                info.path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| "Open file…".into());
        let can_undo = !self.busy && self.info.as_ref().is_some_and(|info| info.can_undo);
        let can_redo = !self.busy && self.info.as_ref().is_some_and(|info| info.can_redo);
        let format = self
            .panels
            .update(cx, |panels, cx| panels.render_format(window, cx));
        let columns = self
            .panels
            .update(cx, |panels, cx| panels.render_columns(window, cx));
        let filters = self
            .panels
            .update(cx, |panels, cx| panels.render_filters(window, cx));
        let mut body = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_size(px(13.))
            .track_focus(&self.focus_handle)
            .key_context("QuarryComparison")
            .on_action(cx.listener(Self::open_action))
            .on_action(cx.listener(Self::save_action))
            .on_action(cx.listener(Self::quit_action))
            .on_action(cx.listener(Self::edit_action))
            .on_action(cx.listener(Self::undo_action))
            .on_action(cx.listener(Self::redo_action))
            .child(
                div()
                    .h(px(42.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .bg(rgb(0xeaf0f2))
                    .child(
                        Button::new("open-file")
                            .label(filename)
                            .ghost()
                            .small()
                            .max_w(px(220.))
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.choose_open(window, cx)),
                            ),
                    )
                    .child(format)
                    .child(div().w(px(1.)).h(px(18.)).mx_1().bg(cx.theme().border))
                    .child(div().text_color(cx.theme().muted_foreground).child("Row"))
                    .child(
                        Input::new(&self.row_input)
                            .aria_label("Go to row")
                            .small()
                            .w(px(64.))
                            .disabled(!loaded),
                    )
                    .child(
                        Button::new("go-row")
                            .label("Go")
                            .ghost()
                            .small()
                            .disabled(!loaded)
                            .on_click(cx.listener(|this, _, _, cx| this.go_to_row(cx))),
                    )
                    .child(
                        Button::new("undo")
                            .label("Undo")
                            .ghost()
                            .small()
                            .disabled(!can_undo)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.command(Command::Undo, cx)),
                            ),
                    )
                    .child(
                        Button::new("redo")
                            .label("Redo")
                            .ghost()
                            .small()
                            .disabled(!can_redo)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.command(Command::Redo, cx)),
                            ),
                    )
                    .child(columns)
                    .child(filters)
                    .child(div().flex_1())
                    .child(
                        Button::new("save-copy")
                            .label("Save a copy…")
                            .ghost()
                            .small()
                            .disabled(!loaded || self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.choose_save(window, cx)),
                            ),
                    ),
            );
        if loaded {
            body = body.child(
                div().flex_1().min_h_0().overflow_hidden().child(
                    DataTable::new(&self.table)
                        .stripe(true)
                        .bordered(false)
                        .small(),
                ),
            );
        } else {
            body = body.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(27.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Quarry"),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child("Open a delimited file to begin."),
                    )
                    .child(
                        Button::new("open-empty")
                            .label("Open file…")
                            .small()
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.choose_open(window, cx)),
                            ),
                    ),
            );
        }
        if let Some((message, is_error)) = &self.message {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_color(if *is_error {
                        rgb(0xb13c35).into()
                    } else {
                        cx.theme().foreground
                    })
                    .child(div().flex_1().min_w_0().child(message.clone()))
                    .child(
                        Button::new("dismiss-message")
                            .label("×")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.message = None;
                                cx.notify();
                            })),
                    ),
            );
        }
        let status = self
            .info
            .as_ref()
            .map(|info| {
                let shown = self.table.read(cx).delegate().columns().len();
                let rows = if info.filter_progress.is_some() {
                    format!(
                        "{} of {} rows",
                        number(info.available_rows),
                        number(info.total_rows)
                    )
                } else {
                    format!("{} rows", number(info.total_rows))
                };
                format!(
                    "{}   ·   {:.2} MiB   ·   {} of {} columns{}",
                    rows,
                    info.file_size as f64 / 1_048_576.,
                    shown,
                    info.headers.len(),
                    if info.dirty {
                        "   ·   Unsaved edits"
                    } else {
                        ""
                    }
                )
            })
            .unwrap_or_else(|| "GPUI comparison".into());
        let progress = if self.busy {
            "Working…".into()
        } else if let Some(info) = &self.info {
            if let Some(progress) = info.filter_progress {
                if progress.done {
                    "Filter complete".into()
                } else {
                    "Filtering…".into()
                }
            } else if info.index_progress.done {
                "Index complete".into()
            } else {
                format!(
                    "Indexing {:.0}%",
                    100. * info.index_progress.bytes_scanned as f64 / info.file_size.max(1) as f64
                )
            }
        } else {
            "⌘O to open".into()
        };
        body.child(
            div()
                .flex()
                .items_center()
                .h(px(30.))
                .flex_shrink_0()
                .px_3()
                .gap_4()
                .border_t_1()
                .border_color(cx.theme().border)
                .bg(rgb(quarry_workbench::theme::LIGHT.chrome.to_rgb_hex()))
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(status)
                .child(div().flex_1())
                .child(progress)
                .child(div().child("GPUI comparison")),
        )
    }
}

fn number(value: u64) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (i, ch) in text.chars().enumerate() {
        if i > 0 && (text.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn bind_shortcuts(cx: &mut App) {
    #[cfg(target_os = "macos")]
    let bindings = [
        KeyBinding::new("cmd-o", OpenFile, None),
        KeyBinding::new("cmd-s", SaveCopy, None),
        KeyBinding::new("cmd-shift-s", SaveCopy, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("f2", EditCell, Some("QuarryComparison")),
        KeyBinding::new("cmd-z", UndoEdit, Some("QuarryComparison && !Input")),
        KeyBinding::new("cmd-shift-z", RedoEdit, Some("QuarryComparison && !Input")),
    ];
    #[cfg(not(target_os = "macos"))]
    let bindings = [
        KeyBinding::new("ctrl-o", OpenFile, None),
        KeyBinding::new("ctrl-s", SaveCopy, None),
        KeyBinding::new("ctrl-shift-s", SaveCopy, None),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("f2", EditCell, Some("QuarryComparison")),
        KeyBinding::new("ctrl-z", UndoEdit, Some("QuarryComparison && !Input")),
        KeyBinding::new("ctrl-shift-z", RedoEdit, Some("QuarryComparison && !Input")),
    ];
    cx.bind_keys(bindings);
}

fn main() {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
            let tokens = &quarry_workbench::theme::LIGHT;
            let theme = Theme::global_mut(cx);
            theme.font_size = px(quarry_workbench::theme::GRID_FONT_SIZE);
            theme.radius = px(f32::from(quarry_workbench::theme::CONTROL_RADIUS));
            theme.radius_lg = px(f32::from(quarry_workbench::theme::WINDOW_RADIUS));
            theme.colors.background = rgb(tokens.chrome.to_rgb_hex()).into();
            theme.colors.foreground = rgb(tokens.text.to_rgb_hex()).into();
            theme.colors.border = rgb(tokens.separator.to_rgb_hex()).into();
            theme.colors.muted_foreground = rgb(tokens.text_secondary.to_rgb_hex()).into();
            theme.colors.primary = rgb(quarry_workbench::theme::ACCENT.to_rgb_hex()).into();
            theme.colors.primary_hover = rgb(0xe4b23c).into();
            theme.colors.primary_active = rgb(0xd8a32e).into();
            theme.colors.primary_foreground = rgb(tokens.on_accent.to_rgb_hex()).into();
            theme.colors.table = rgb(tokens.canvas.to_rgb_hex()).into();
            theme.colors.table_even = rgb(tokens.zebra.to_rgb_hex()).into();
            theme.colors.table_head = rgb(tokens.header_fill.to_rgb_hex()).into();
            theme.colors.table_head_foreground = rgb(tokens.text_secondary.to_rgb_hex()).into();
            theme.colors.table_row_border = rgb(tokens.grid_line.to_rgb_hex()).into();
            theme.colors.table_active = rgb(tokens.selection.to_rgb_hex()).into();
            theme.colors.table_active_border = rgb(tokens.selection_strong.to_rgb_hex()).into();
            theme.colors.popover = rgb(tokens.canvas.to_rgb_hex()).into();
            theme.colors.button_primary = theme.colors.primary;
            theme.colors.button_primary_hover = theme.colors.primary_hover;
            theme.colors.button_primary_active = theme.colors.primary_active;
            theme.colors.button_primary_foreground = theme.colors.primary_foreground;
            theme.tokens = theme.colors.into();
            Theme::sync_base(cx);
            bind_shortcuts(cx);
            cx.set_menus(vec![Menu {
                name: "Quarry GPUI".into(),
                disabled: false,
                items: vec![
                    MenuItem::action("Open…", OpenFile),
                    MenuItem::action("Save a copy…", SaveCopy),
                    MenuItem::separator(),
                    MenuItem::action("Quit Quarry GPUI", Quit),
                ],
            }]);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.spawn(async move |cx| {
                let bounds = cx.update(|cx| Bounds::centered(None, size(px(1280.), px(800.)), cx));
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_min_size: Some(size(px(860.), px(500.))),
                        titlebar: Some(TitlebarOptions {
                            title: Some("Quarry GPUI comparison".into()),
                            ..Default::default()
                        }),
                        app_id: Some("dev.quarry.gpui-comparison".into()),
                        ..Default::default()
                    },
                    move |window, cx| {
                        let view = cx.new(|cx| Quarry::new(window, cx));
                        if let Some(path) = path {
                            view.update(cx, |this, cx| {
                                this.command(Command::Open(path, OpenOptions::default()), cx)
                            });
                        }
                        cx.new(|cx| Root::new(view, window, cx))
                    },
                )
                .expect("Could not open the GPUI comparison window");
            })
            .detach();
            cx.activate(true);
        });
}
