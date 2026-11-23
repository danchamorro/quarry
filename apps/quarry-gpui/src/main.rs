mod find_bar;
mod grid;
mod menus;
mod operations;
mod panels;
mod workbench;
mod workflows_ui;

#[cfg(test)]
mod ui_tests;

use std::path::PathBuf;

#[cfg(not(target_os = "macos"))]
use gpui_kit::component::menu::AppMenuBar;
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
use quarry_core::{Encoding, OpenOptions};
use quarry_workbench::{
    AppMessage, CommandId, DocumentId, FailedOpen, MessageSeverity, ResidencyState, ResolvedTheme,
    ThemeAppearance, ThemePreference, WindowId, compressed::uncompressed_name, format_bytes,
};

use find_bar::{FindBar, FindBarEvent};
use grid::Grid;
use menus::{
    AboutQuarry, CleanupColumns, CloseTab, CloseWindow, CombineColumns, CombineFiles,
    CopySelection, DeleteSelectedColumns, DeleteSelectedRows, DiscardChanges, EditCell,
    ExportFiltered, FindDuplicates, FindNext, FindPrevious, FindText, InsertColumns, InsertRows,
    InspectFile, JoinFiles, MoveColumns, MoveTabToNewWindow, NewWindow, NextTab, OpenFile,
    OpenIso88591, OpenUtf8, OpenUtf16Be, OpenUtf16Le, OpenWindows1252, PreviousTab,
    QUARRY_KEY_CONTEXT, QuitApp, RedoEdit, ReloadFromDisk, RenameHeader, ReplaceText, SaveAsFile,
    SaveAsUtf8, SaveFile, ShowAllWhitespace, SortRows, SplitColumn, TemporaryStorage, ThemeDark,
    ThemeLight, ThemeOmarchy, ThemeSystem, ToggleColumns, ToggleFilters, UndoEdit, bind_shortcuts,
    encoding_label, install_menus,
};
use panels::{PanelEvent, Panels, Tool};
use workbench::{
    CloseReviewAction, DocumentSnapshot, MAX_EDIT_BYTES, WorkbenchController, WorkflowDialogKind,
};

pub struct QuarryWindow {
    window_id: WindowId,
    controller: Entity<WorkbenchController>,
    focus_handle: FocusHandle,
    #[cfg(not(target_os = "macos"))]
    menu_bar: Entity<AppMenuBar>,
    table: Entity<TableState<Grid>>,
    panels: Entity<Panels>,
    find_bar: Entity<FindBar>,
    row_input: Entity<InputState>,
    last_synced_doc: Option<(DocumentId, u64)>,
    last_headers: Vec<String>,
    last_shown_columns: Vec<usize>,
    confirming: bool,
    editing: bool,
    editor_status: Option<Entity<CellEditStatus>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
struct CellEditStatus {
    error: Option<String>,
}

impl QuarryWindow {
    fn new(
        window_id: WindowId,
        controller: Entity<WorkbenchController>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        #[cfg(not(target_os = "macos"))]
        let menu_bar = AppMenuBar::new(cx);
        let table = cx.new(|cx| {
            TableState::new(Grid::new(controller.clone(), window_id), window, cx)
                .cell_selectable(true)
                .row_header(false)
                .col_movable(false)
        });
        let panels = cx.new(|cx| Panels::new(window, cx));
        let find_bar = cx.new(|cx| FindBar::new(controller.clone(), window_id, window, cx));
        let row_input = cx.new(|cx| InputState::new(window, cx).default_value("1"));
        let initial_appearance = window_appearance_to_theme(window.appearance());
        if let Some(resolved) =
            controller.update(cx, |c, cx| c.sync_system_appearance(initial_appearance, cx))
        {
            apply_resolved_theme(&resolved, cx);
        }
        let subscriptions = vec![
            cx.observe_window_appearance(window, |this, window, cx| {
                let appearance = window_appearance_to_theme(window.appearance());
                if let Some(resolved) = this
                    .controller
                    .update(cx, |c, cx| c.sync_system_appearance(appearance, cx))
                {
                    apply_resolved_theme(&resolved, cx);
                    cx.refresh_windows();
                }
            }),
            cx.observe_in(&controller, window, |this, _, window, cx| {
                this.sync_from_controller(window, cx);
            }),
            cx.subscribe_in(
                &find_bar,
                window,
                |this, _, event: &FindBarEvent, window, cx| {
                    if matches!(event, FindBarEvent::Closed) {
                        let table_focus = this
                            .snapshot(cx)
                            .is_some()
                            .then(|| this.table.focus_handle(cx));
                        let root_focus = this.focus_handle.clone();
                        window.defer(cx, move |window, cx| {
                            if let Some(tf) = table_focus {
                                tf.focus(window, cx);
                            } else {
                                root_focus.focus(window, cx);
                            }
                        });
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &table,
                window,
                |this, table, event, window, cx| match event {
                    TableEvent::SelectCell(row, column) => {
                        let target = table.read(cx).delegate().cell_at(*row, *column, cx);
                        this.controller.update(cx, |controller, _| {
                            controller.set_active_selection(
                                this.window_id,
                                target.map(|(r, c)| quarry_workbench::GridSelection::Cell {
                                    row: r,
                                    column: c,
                                }),
                            );
                        });
                    }
                    TableEvent::DoubleClickedCell(row, column) => {
                        let target = table.read(cx).delegate().cell_at(*row, *column, cx);
                        this.controller.update(cx, |controller, _| {
                            controller.set_active_selection(
                                this.window_id,
                                target.map(|(r, c)| quarry_workbench::GridSelection::Cell {
                                    row: r,
                                    column: c,
                                }),
                            );
                        });
                        this.edit_cell(*row, *column, window, cx);
                    }
                    TableEvent::ColumnWidthsChanged(widths) => {
                        let widths = widths.clone();
                        this.table.update(cx, |table, cx| {
                            table.delegate_mut().set_widths(&widths, cx);
                        });
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(&panels, window, |this, _, event, window, cx| {
                match event {
                    PanelEvent::Columns(columns) => {
                        this.controller.update(cx, |controller, cx| {
                            controller.set_active_shown_columns(
                                this.window_id,
                                columns.clone(),
                                cx,
                            );
                        });
                        this.table.update(cx, |table, cx| {
                            table.refresh(cx);
                            cx.notify();
                        });
                    }
                    PanelEvent::AutoFit => this.table.update(cx, |table, cx| {
                        table.delegate_mut().auto_fit(cx);
                        table.refresh(cx);
                        cx.notify();
                    }),
                    PanelEvent::Filter(query) => {
                        this.table
                            .update(cx, |table, cx| table.scroll_to_row(0, cx));
                        this.row_input
                            .update(cx, |state, cx| state.set_value("1", window, cx));
                        let res = this.controller.update(cx, |controller, cx| {
                            controller.start_filter_active(this.window_id, query.clone(), cx)
                        });
                        if let Err(error) = res {
                            this.set_message(AppMessage::error(error), cx);
                        }
                    }
                    PanelEvent::ClearFilter => {
                        this.table
                            .update(cx, |table, cx| table.scroll_to_row(0, cx));
                        this.row_input
                            .update(cx, |state, cx| state.set_value("1", window, cx));
                        let visible_rows = this.table.read(cx).delegate().visible_rows_count();
                        let res = this.controller.update(cx, |controller, cx| {
                            let cleared = controller.clear_filter_active(this.window_id, cx);
                            if cleared.is_ok() {
                                controller.request_viewport_rows(
                                    this.window_id,
                                    0,
                                    visible_rows,
                                    cx,
                                );
                            }
                            cleared
                        });
                        if let Err(error) = res {
                            this.set_message(AppMessage::error(error), cx);
                        }
                    }
                    PanelEvent::ExportFiltered => {
                        this.choose_export_filtered_destination(window, cx);
                    }
                    PanelEvent::CancelFilterOrExport => {
                        this.controller.update(cx, |controller, cx| {
                            controller.cancel_filter_or_export_active(this.window_id, cx);
                        });
                    }
                    PanelEvent::Reopen(options, encoding) => {
                        let res = this.controller.update(cx, |controller, cx| {
                            controller.reopen_active_tab(this.window_id, *options, *encoding, cx)
                        });
                        if let Err(error) = res {
                            this.set_message(AppMessage::warning(error), cx);
                        } else {
                            this.table.update(cx, |table, cx| {
                                table.delegate_mut().reset_widths();
                                table.refresh(cx);
                                table.scroll_to_row(0, cx);
                                table.scroll_to_col(0, cx);
                                cx.notify();
                            });
                        }
                    }
                }
                cx.notify();
            }),
            cx.subscribe_in(&row_input, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.go_to_row(cx);
                }
            }),
        ];
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |this, cx| {
                this.close_window_requested(window, cx);
                false
            })
            .unwrap_or(true)
        });
        Self {
            window_id,
            controller,
            focus_handle,
            #[cfg(not(target_os = "macos"))]
            menu_bar,
            table,
            panels,
            find_bar,
            row_input,
            last_synced_doc: None,
            last_headers: Vec::new(),
            last_shown_columns: Vec::new(),
            confirming: false,
            editing: false,
            editor_status: None,
            _subscriptions: subscriptions,
        }
    }

    fn set_message(&mut self, message: AppMessage, cx: &mut Context<Self>) {
        self.controller.update(cx, |controller, cx| {
            controller.set_window_message(self.window_id, Some(message), cx);
        });
    }

    fn snapshot(&self, cx: &App) -> Option<DocumentSnapshot> {
        self.controller.read(cx).active_snapshot(self.window_id)
    }

    fn command_enabled(&self, command: CommandId, cx: &App) -> bool {
        self.controller
            .read(cx)
            .evaluate_window_command(
                self.window_id,
                command,
                self.confirming || self.editing,
                false,
            )
            .enabled
    }

    fn sync_from_controller(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (close_windows, quit_done) = self.controller.update(cx, |controller, _| {
            (
                controller.take_pending_close_windows(),
                controller.take_quit_complete(),
            )
        });
        if quit_done {
            cx.quit();
            return;
        }
        for win_id in close_windows {
            if win_id == self.window_id {
                self.controller.update(cx, |controller, _| {
                    controller.unregister_window_handle(self.window_id);
                });
                window.remove_window();
                return;
            } else if let Some(handle) = self.controller.read(cx).window_handle(win_id) {
                self.controller.update(cx, |controller, _| {
                    controller.unregister_window_handle(win_id);
                });
                let _ = handle.update(cx, |_, other_window, _| other_window.remove_window());
            }
        }

        if let Some(info) = self.snapshot(cx) {
            let doc_key = (info.document_id, info.instance_id);
            let doc_changed = self.last_synced_doc != Some(doc_key);
            let schema_changed = doc_changed
                || self.last_headers != info.headers
                || self.last_shown_columns != info.shown_columns;
            window.set_window_title(&format!(
                "{}{} · Quarry",
                info.filename,
                if info.dirty { " *" } else { "" }
            ));
            self.panels
                .update(cx, |panels, cx| panels.sync_document(&info, window, cx));
            if schema_changed {
                self.last_synced_doc = Some(doc_key);
                self.last_headers = info.headers.clone();
                self.last_shown_columns = info.shown_columns.clone();
                self.table.update(cx, |table, cx| {
                    if doc_changed {
                        table.delegate_mut().reset_widths();
                        table.scroll_to_row(0, cx);
                        table.scroll_to_col(0, cx);
                    }
                    table.refresh(cx);
                    cx.notify();
                });
                if doc_changed {
                    let visible_rows = self.table.read(cx).delegate().visible_rows_count();
                    self.controller.update(cx, |controller, cx| {
                        controller.request_viewport_rows(self.window_id, 0, visible_rows, cx);
                    });
                    self.row_input
                        .update(cx, |state, cx| state.set_value("1", window, cx));
                }
            } else {
                self.table.update(cx, |_, cx| cx.notify());
            }
        } else {
            if self.last_synced_doc.is_some() {
                self.last_synced_doc = None;
                self.last_headers.clear();
                self.last_shown_columns.clear();
                self.panels
                    .update(cx, |panels, cx| panels.clear_document(window, cx));
                self.table.update(cx, |table, cx| {
                    table.delegate_mut().reset_widths();
                    table.refresh(cx);
                    cx.notify();
                });
            }
            window.set_window_title("Quarry");
        }
        let pending_reveal = self.controller.update(cx, |controller, _| {
            controller.take_pending_reveal_cell(self.window_id)
        });
        if let Some((source_row, source_col)) = pending_reveal
            && let Some((table_row, table_col)) = self
                .table
                .read(cx)
                .delegate()
                .display_position_for_source(source_row, source_col, cx)
        {
            self.table.update(cx, |table, cx| {
                table.scroll_to_row(table_row, cx);
                table.scroll_to_col(table_col, cx);
                table.set_selected_cell(table_row, table_col, cx);
                cx.notify();
            });
            self.row_input.update(cx, |state, cx| {
                state.set_value((table_row + 1).to_string(), window, cx);
            });
        }
        let pending_action = self.controller.update(cx, |controller, _| {
            controller.take_pending_review_action(self.window_id)
        });
        if let Some(action) = pending_action {
            self.handle_review_action(action, window, cx);
        }
        cx.notify();
    }

    pub fn open_paths_direct(
        &mut self,
        paths: Vec<PathBuf>,
        options: OpenOptions,
        encoding: Option<Encoding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self.controller.update(cx, |controller, cx| {
            controller.open_paths(self.window_id, paths, options, encoding, cx)
        });
        if let Err(message) = result {
            self.set_message(message, cx);
        } else {
            self.sync_from_controller(window, cx);
        }
    }

    fn choose_open_with_encoding(
        &mut self,
        encoding: Option<Encoding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming || self.editing {
            return;
        }
        self.confirming = true;
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open in Quarry".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let selection = selection.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.confirming = false;
                match selection {
                    Ok(Ok(Some(paths))) if !paths.is_empty() => {
                        this.open_paths_direct(paths, OpenOptions::default(), encoding, window, cx);
                    }
                    Ok(Err(error)) => {
                        this.set_message(AppMessage::error(error.to_string()), cx);
                    }
                    _ => {}
                }
            });
        })
        .detach();
    }

    fn choose_save_destination(
        &mut self,
        encoding: Option<Encoding>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming || self.editing {
            return;
        }
        let Some(info) = self.snapshot(cx) else {
            return;
        };
        let target_doc_id = info.document_id;
        let suggested_base = uncompressed_name(&info.path);
        let directory = suggested_base
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        let suffix = if encoding == Some(Encoding::Utf8) {
            "-utf8"
        } else {
            "-copy"
        };
        let name = format!(
            "{}{}.{}",
            suggested_base
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy(),
            suffix,
            suggested_base
                .extension()
                .unwrap_or_else(|| std::ffi::OsStr::new("csv"))
                .to_string_lossy()
        );
        let selection = cx.prompt_for_new_path(&directory, Some(&name));
        self.confirming = true;
        cx.spawn_in(window, async move |this, cx| {
            let selection = selection.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.confirming = false;
                match selection {
                    Ok(Ok(Some(path))) => {
                        let res = this.controller.update(cx, |controller, cx| {
                            controller.start_save_active(
                                this.window_id,
                                Some(target_doc_id),
                                Some(path),
                                encoding,
                                cx,
                            )
                        });
                        if let Err(error) = res {
                            this.set_message(error, cx);
                        }
                    }
                    Ok(Err(error)) => {
                        this.set_message(AppMessage::error(error.to_string()), cx);
                    }
                    _ => {}
                }
            });
        })
        .detach();
    }

    fn save_in_place(&mut self, cx: &mut Context<Self>) {
        if !self.command_enabled(CommandId::Save, cx) {
            return;
        }
        let res = self.controller.update(cx, |controller, cx| {
            controller.start_save_active(self.window_id, None, None, None, cx)
        });
        if let Err(error) = res {
            self.set_message(error, cx);
        }
    }

    fn handle_review_action(
        &mut self,
        action: CloseReviewAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            CloseReviewAction::Proceed => {
                self.sync_from_controller(window, cx);
            }
            CloseReviewAction::Wait { message } => {
                self.set_message(AppMessage::warning(message), cx);
            }
            CloseReviewAction::ConfirmTab {
                review_token,
                window_id,
                document_id,
                filename,
            } => {
                if window_id != self.window_id
                    && self.controller.read(cx).window_handle(window_id).is_some()
                {
                    self.controller.update(cx, |controller, cx| {
                        controller.queue_review_action(
                            CloseReviewAction::ConfirmTab {
                                review_token,
                                window_id,
                                document_id,
                                filename,
                            },
                            cx,
                        );
                    });
                    return;
                }
                self.show_unsaved_close_dialog(review_token, document_id, filename, window, cx);
            }
        }
    }

    fn show_unsaved_close_dialog(
        &mut self,
        review_token: u64,
        document_id: DocumentId,
        filename: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming {
            return;
        }
        self.confirming = true;
        self.panels
            .update(cx, |panels, cx| panels.close(window, cx));
        let controller = self.controller.clone();
        let status = cx.new(|_| CellEditStatus::default());
        let this_weak = cx.weak_entity();
        let keep_weak = cx.weak_entity();
        let discard_weak = cx.weak_entity();
        let save_weak = cx.weak_entity();
        let close_weak = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, cx| {
            let filename_msg = filename.clone();
            let controller_keep = controller.clone();
            let controller_discard = controller.clone();
            let controller_save = controller.clone();
            let controller_cancel = controller.clone();
            let keep_weak = keep_weak.clone();
            let discard_weak = discard_weak.clone();
            let save_weak = save_weak.clone();
            let close_weak = close_weak.clone();
            let save_status = status.clone();
            let error = status.read(cx).error.clone();
            let _ = &this_weak;
            dialog
                .title(format!("Save changes to {filename}?"))
                .w(px(460.))
                .overlay_closable(false)
                .close_button(true)
                .keyboard(true)
                .content(move |content, _, cx| {
                    content
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{filename_msg} has unsaved edits. Save your changes, discard them, or keep editing."
                                )),
                        )
                        .when_some(error.clone(), |content, error| {
                            content.child(
                                div()
                                    .id("close-review-error")
                                    .role(Role::Alert)
                                    .text_color(cx.theme().danger)
                                    .child(error),
                            )
                        })
                })
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("close-keep-editing")
                                .label("Keep Editing")
                                .small()
                                .on_click(move |_, window, cx| {
                                    controller_keep.update(cx, |c, cx| {
                                        c.cancel_close_review(Some(review_token), cx)
                                    });
                                    let _ = keep_weak.update(cx, |this, _| {
                                        this.confirming = false;
                                    });
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("close-discard")
                                .label("Discard")
                                .danger()
                                .small()
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let next = controller_discard.update(cx, |c, cx| {
                                        c.approve_discard_in_review(review_token, cx)
                                    });
                                    let _ = discard_weak.update(cx, |this, cx| {
                                        this.confirming = false;
                                        this.handle_review_action(next, window, cx);
                                    });
                                }),
                        )
                        .child(
                            Button::new("close-save")
                                .label("Save")
                                .primary()
                                .small()
                                .on_click(move |_, window, cx| {
                                    let res = controller_save.update(cx, |c, cx| {
                                        c.save_in_review(review_token, document_id, cx)
                                    });
                                    match res {
                                        Ok(()) => {
                                            window.close_dialog(cx);
                                            let _ = save_weak.update(cx, |this, _| {
                                                this.confirming = false;
                                            });
                                        }
                                        Err(error) => {
                                            save_status.update(cx, |st, cx| {
                                                st.error = Some(error.text.clone());
                                                cx.notify();
                                            });
                                            let _ = save_weak.update(cx, |this, cx| {
                                                this.set_message(error, cx);
                                            });
                                            window.refresh();
                                        }
                                    }
                                }),
                        ),
                )
                .on_cancel(move |_, _, cx| {
                    controller_cancel
                        .update(cx, |c, cx| c.cancel_close_review(Some(review_token), cx));
                    true
                })
                .on_close(move |_, _, cx| {
                    let _ = close_weak.update(cx, |this, cx| {
                        this.confirming = false;
                        cx.notify();
                    });
                })
        });
    }

    fn close_tab_by_id(
        &mut self,
        id: DocumentId,
        close_window_if_last: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming || self.editing {
            return;
        }
        let action = self.controller.update(cx, |controller, cx| {
            controller.request_close_tab(self.window_id, id, close_window_if_last, cx)
        });
        self.handle_review_action(action, window, cx);
    }

    fn close_window_requested(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming || self.editing {
            return;
        }
        let action = self.controller.update(cx, |controller, cx| {
            controller.request_close_window(self.window_id, cx)
        });
        self.handle_review_action(action, window, cx);
    }

    fn quit_requested(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming || self.editing {
            return;
        }
        let action = self
            .controller
            .update(cx, |controller, cx| controller.request_quit(cx));
        self.handle_review_action(action, window, cx);
    }

    fn go_to_row(&mut self, cx: &mut Context<Self>) {
        let limit = self.snapshot(cx).map_or(0, |info| info.available_rows);
        match self.row_input.read(cx).value().parse::<u64>() {
            Ok(row) if row > 0 && row <= limit => {
                self.table
                    .update(cx, |table, cx| table.scroll_to_row((row - 1) as usize, cx));
                self.controller.update(cx, |controller, cx| {
                    controller.set_window_message(self.window_id, None, cx);
                });
            }
            _ => self.set_message(
                AppMessage::error(format!("Enter a row from 1 to {limit}.")),
                cx,
            ),
        }
        cx.notify();
    }

    fn edit_cell(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        let Some((source_row, source_col)) =
            self.table.read(cx).delegate().cell_at(row, column, cx)
        else {
            return;
        };
        let (doc_id, title, initial_value, disabled_reason) = {
            let controller = self.controller.read(cx);
            let Some(doc_id) = controller.scheduler().active_document(self.window_id) else {
                return;
            };
            let Some(doc) = controller.active_document(self.window_id) else {
                return;
            };
            let source = doc.source_cell(source_row, source_col);
            let disabled = doc.cell_edit_disabled_reason(source);
            let value_bytes = doc.effective_cell(source_row, source_col).unwrap_or(&[]);
            let value = String::from_utf8_lossy(value_bytes).into_owned();
            (doc_id, doc.column_name(source_col), value, disabled)
        };
        if let Some(reason) = disabled_reason {
            self.set_message(AppMessage::warning(reason), cx);
            return;
        }
        self.show_editor(
            doc_id,
            source_row,
            source_col,
            title,
            initial_value,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn show_editor(
        &mut self,
        doc_id: DocumentId,
        row: u64,
        column: usize,
        title: String,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| TextareaState::new(window, cx).default_value(value));
        self.panels
            .update(cx, |panels, cx| panels.close(window, cx));
        self.editing = true;
        self.controller.update(cx, |controller, cx| {
            controller.set_window_editing(self.window_id, true, cx);
        });
        let status = cx.new(|_| CellEditStatus::default());
        self.editor_status = Some(status.clone());
        let focus_input = input.clone();
        let apply = cx.weak_entity();
        let close = cx.weak_entity();
        if self.snapshot(cx).is_some() {
            self.table.focus_handle(cx).focus(window, cx);
        }
        window.open_dialog(cx, move |dialog, _, cx| {
            let input_value = input.clone();
            let input_view = input.clone();
            let apply = apply.clone();
            let close = close.clone();
            let error = status.read(cx).error.clone();
            let apply_status = status.clone();
            dialog
                .title(format!("{} · source row {}", title, row + 1))
                .w(px(480.))
                .overlay_closable(false)
                .close_button(true)
                .keyboard(true)
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("cancel-cell-edit")
                                .label("Cancel")
                                .small()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(DialogCancel), cx)
                                }),
                        )
                        .child(
                            Button::new("apply-cell-edit")
                                .label("Apply edit")
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
                .content(move |content, _, cx| {
                    let too_large = input_view.read(cx).value().len() > MAX_EDIT_BYTES;
                    content
                        .child(
                            Textarea::new(&input_view)
                                .aria_label("Cell value")
                                .h(px(116.)),
                        )
                        .when(too_large, |content| {
                            content.child(
                                div()
                                    .id("cell-edit-error")
                                    .role(Role::Alert)
                                    .test_support()
                                    .text_color(cx.theme().danger)
                                    .child(
                                        "Cell values must be 1 MiB or smaller. Shorten the value to apply it.",
                                    ),
                            )
                        })
                        .when_some(error.clone().filter(|_| !too_large), |content, error| {
                            content.child(
                                div()
                                    .id("cell-edit-error")
                                    .role(Role::Alert)
                                    .test_support()
                                    .text_color(cx.theme().danger)
                                    .child(error),
                            )
                        })
                })
                .on_ok(move |_, window, cx| {
                    let value = input_value.read(cx).value().to_string();
                    if value.len() > MAX_EDIT_BYTES {
                        window.refresh();
                        return false;
                    }
                    let outcome = apply.update(cx, |this, cx| {
                        this.controller.update(cx, |controller, cx| {
                            controller.apply_cell_edit(this.window_id, doc_id, row, column, value, cx)
                        })
                    });
                    match outcome {
                        Ok(Ok(())) => true,
                        Ok(Err(err)) => {
                            apply_status.update(cx, |status, cx| {
                                status.error = Some(err);
                                cx.notify();
                            });
                            window.refresh();
                            false
                        }
                        Err(_) => true,
                    }
                })
                .on_close(move |_, window, cx| {
                    let _ = close.update(cx, |this, cx| {
                        this.editing = false;
                        this.editor_status = None;
                        this.controller.update(cx, |controller, cx| {
                            controller.set_window_editing(this.window_id, false, cx);
                        });
                        let table_focus = this
                            .snapshot(cx)
                            .is_some()
                            .then(|| this.table.focus_handle(cx));
                        let root_focus = this.focus_handle.clone();
                        window.defer(cx, move |window, cx| {
                            if let Some(table_focus) = table_focus {
                                table_focus.focus(window, cx);
                            } else {
                                root_focus.focus(window, cx);
                            }
                        });
                        cx.notify();
                    });
                })
        });
        focus_input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn show_header_rename_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing || self.confirming {
            return;
        }
        let selected_col = self
            .table
            .read(cx)
            .selected_cell()
            .and_then(|(_, col)| self.table.read(cx).delegate().source_column_at(col, cx))
            .or_else(|| self.table.read(cx).delegate().columns(cx).first().copied());
        let Some(source_col) = selected_col else {
            return;
        };
        let (doc_id, editable, current_name) = {
            let controller = self.controller.read(cx);
            let Some(doc_id) = controller.scheduler().active_document(self.window_id) else {
                return;
            };
            let Some(doc) = controller.active_document(self.window_id) else {
                return;
            };
            (
                doc_id,
                doc.header_is_editable(source_col),
                doc.column_name(source_col),
            )
        };
        if !editable {
            self.set_message(
                AppMessage::warning("Header renaming is unavailable for this document."),
                cx,
            );
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current_name.clone()));
        self.editing = true;
        self.controller.update(cx, |controller, cx| {
            controller.set_window_editing(self.window_id, true, cx);
        });
        let focus_input = input.clone();
        let apply = cx.weak_entity();
        let close = cx.weak_entity();
        let status = cx.new(|_| CellEditStatus::default());
        self.table.focus_handle(cx).focus(window, cx);
        window.open_dialog(cx, move |dialog, _, cx| {
            let input_value = input.clone();
            let input_view = input.clone();
            let apply = apply.clone();
            let close = close.clone();
            let error = status.read(cx).error.clone();
            let apply_status = status.clone();
            dialog
                .title(format!("Rename column {}", source_col + 1))
                .w(px(400.))
                .overlay_closable(false)
                .close_button(true)
                .keyboard(true)
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("cancel-header-rename")
                                .label("Cancel")
                                .small()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(DialogCancel), cx)
                                }),
                        )
                        .child(
                            Button::new("apply-header-rename")
                                .label("Rename")
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
                .content(move |content, _, cx| {
                    content
                        .child(Input::new(&input_view).aria_label("Column header name"))
                        .when_some(error.clone(), |content, error| {
                            content.child(
                                div()
                                    .id("header-rename-error")
                                    .role(Role::Alert)
                                    .text_color(cx.theme().danger)
                                    .child(error),
                            )
                        })
                })
                .on_ok(move |_, window, cx| {
                    let name = input_value.read(cx).value().to_string();
                    let outcome = apply.update(cx, |this, cx| {
                        this.controller.update(cx, |controller, cx| {
                            controller.rename_header_active(
                                this.window_id,
                                doc_id,
                                source_col,
                                name,
                                cx,
                            )
                        })
                    });
                    match outcome {
                        Ok(Ok(())) => true,
                        Ok(Err(err)) => {
                            apply_status.update(cx, |status, cx| {
                                status.error = Some(err);
                                cx.notify();
                            });
                            window.refresh();
                            false
                        }
                        Err(_) => true,
                    }
                })
                .on_close(move |_, window, cx| {
                    let _ = close.update(cx, |this, cx| {
                        this.editing = false;
                        this.controller.update(cx, |controller, cx| {
                            controller.set_window_editing(this.window_id, false, cx);
                        });
                        let table_focus = this
                            .snapshot(cx)
                            .is_some()
                            .then(|| this.table.focus_handle(cx));
                        window.defer(cx, move |window, cx| {
                            if let Some(table_focus) = table_focus {
                                table_focus.focus(window, cx);
                            }
                        });
                        cx.notify();
                    });
                })
        });
        focus_input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn new_window_action(&mut self, _: &NewWindow, _: &mut Window, cx: &mut Context<Self>) {
        open_quarry_window(self.controller.clone(), Vec::new(), None, cx);
    }

    fn open_action(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_open_with_encoding(None, window, cx);
    }
    fn open_utf8(&mut self, _: &OpenUtf8, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_open_with_encoding(Some(Encoding::Utf8), window, cx);
    }
    fn open_utf16le(&mut self, _: &OpenUtf16Le, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_open_with_encoding(Some(Encoding::Utf16Le), window, cx);
    }
    fn open_utf16be(&mut self, _: &OpenUtf16Be, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_open_with_encoding(Some(Encoding::Utf16Be), window, cx);
    }
    fn open_win1252(&mut self, _: &OpenWindows1252, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_open_with_encoding(Some(Encoding::Windows1252), window, cx);
    }
    fn open_iso88591(&mut self, _: &OpenIso88591, window: &mut Window, cx: &mut Context<Self>) {
        self.choose_open_with_encoding(Some(Encoding::Latin1), window, cx);
    }

    fn close_tab_action(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active_id) = self
            .controller
            .read(cx)
            .scheduler()
            .active_document(self.window_id)
        {
            self.close_tab_by_id(active_id, true, window, cx);
        } else {
            self.close_window_requested(window, cx);
        }
    }

    fn close_window_action(
        &mut self,
        _: &CloseWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_window_requested(window, cx);
    }

    fn move_tab_to_new_window_action(
        &mut self,
        _: &MoveTabToNewWindow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.command_enabled(CommandId::MoveTabToNewWindow, cx) {
            return;
        }
        let Some(active_id) = self
            .controller
            .read(cx)
            .scheduler()
            .active_document(self.window_id)
        else {
            return;
        };
        open_quarry_window(self.controller.clone(), Vec::new(), Some(active_id), cx);
    }

    fn next_tab_action(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        if self.command_enabled(CommandId::NextTab, cx) {
            self.controller.update(cx, |controller, cx| {
                controller.cycle_tab(self.window_id, 1, cx);
            });
        }
    }

    fn previous_tab_action(&mut self, _: &PreviousTab, _: &mut Window, cx: &mut Context<Self>) {
        if self.command_enabled(CommandId::PreviousTab, cx) {
            self.controller.update(cx, |controller, cx| {
                controller.cycle_tab(self.window_id, -1, cx);
            });
        }
    }

    fn reload_action(&mut self, _: &ReloadFromDisk, _: &mut Window, cx: &mut Context<Self>) {
        if !self.command_enabled(CommandId::ReloadFromDisk, cx) {
            return;
        }
        let res = self.controller.update(cx, |controller, cx| {
            controller.reload_active_tab(self.window_id, cx)
        });
        if let Err(error) = res {
            self.set_message(AppMessage::warning(error), cx);
        }
    }

    fn save_action(&mut self, _: &SaveFile, _: &mut Window, cx: &mut Context<Self>) {
        self.save_in_place(cx);
    }

    fn save_as_action(&mut self, _: &SaveAsFile, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_enabled(CommandId::SaveAs, cx) {
            self.choose_save_destination(None, window, cx);
        }
    }

    fn save_as_utf8_action(&mut self, _: &SaveAsUtf8, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_enabled(CommandId::SaveAsUtf8, cx) {
            self.choose_save_destination(Some(Encoding::Utf8), window, cx);
        }
    }

    fn discard_action(&mut self, _: &DiscardChanges, _: &mut Window, cx: &mut Context<Self>) {
        if !self.command_enabled(CommandId::DiscardChanges, cx) {
            return;
        }
        let res = self.controller.update(cx, |controller, cx| {
            controller.discard_active_edits(self.window_id, cx)
        });
        if let Err(error) = res {
            self.set_message(error, cx);
        }
    }

    fn undo_action(&mut self, _: &UndoEdit, _: &mut Window, cx: &mut Context<Self>) {
        if !self.editing && self.command_enabled(CommandId::Undo, cx) {
            let res = self.controller.update(cx, |controller, cx| {
                controller.undo_redo_active(self.window_id, false, cx)
            });
            if let Err(error) = res {
                self.set_message(error, cx);
            }
        }
    }

    fn redo_action(&mut self, _: &RedoEdit, _: &mut Window, cx: &mut Context<Self>) {
        if !self.editing && self.command_enabled(CommandId::Redo, cx) {
            let res = self.controller.update(cx, |controller, cx| {
                controller.undo_redo_active(self.window_id, true, cx)
            });
            if let Err(error) = res {
                self.set_message(error, cx);
            }
        }
    }

    fn copy_action(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        if self.editing {
            return;
        }
        if let Some((row, col)) = self.table.read(cx).selected_cell() {
            let target = self.table.read(cx).delegate().cell_at(row, col, cx);
            self.controller.update(cx, |controller, _| {
                controller.set_active_selection(
                    self.window_id,
                    target
                        .map(|(r, c)| quarry_workbench::GridSelection::Cell { row: r, column: c }),
                );
            });
        }
        match self
            .controller
            .read(cx)
            .copy_active_selection(self.window_id)
        {
            Ok(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.set_message(AppMessage::status("Copied selection."), cx);
            }
            Err(error) => {
                self.set_message(AppMessage::warning(error), cx);
            }
        }
    }

    fn edit_action(&mut self, _: &EditCell, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((row, column)) = self.table.read(cx).selected_cell() {
            self.edit_cell(row, column, window, cx);
        }
    }

    fn rename_header_action(
        &mut self,
        _: &RenameHeader,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_header_rename_dialog(window, cx);
    }

    fn toggle_whitespace_action(
        &mut self,
        _: &ShowAllWhitespace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.controller.update(cx, |controller, cx| {
            controller.toggle_show_all_whitespace(cx);
        });
        self.table.update(cx, |_, cx| cx.notify());
    }

    fn toggle_columns_action(
        &mut self,
        _: &ToggleColumns,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.command_enabled(CommandId::Columns, cx) {
            self.panels
                .update(cx, |panels, cx| panels.open_tool(Tool::Columns, window, cx));
        }
    }

    fn toggle_filters_action(
        &mut self,
        _: &ToggleFilters,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.command_enabled(CommandId::Filters, cx) {
            self.panels
                .update(cx, |panels, cx| panels.open_tool(Tool::Filters, window, cx));
        }
    }

    fn quit_action(&mut self, _: &QuitApp, window: &mut Window, cx: &mut Context<Self>) {
        self.quit_requested(window, cx);
    }

    fn choose_export_filtered_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming || self.editing {
            return;
        }
        let Some(info) = self.snapshot(cx) else {
            return;
        };
        if !info.can_export_filtered {
            self.set_message(
                AppMessage::warning("Wait for filtering to complete before exporting."),
                cx,
            );
            return;
        }
        let target_doc_id = info.document_id;
        let suggested_base = uncompressed_name(&info.path);
        let directory = suggested_base
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        let name = format!(
            "{}-filtered.{}",
            suggested_base
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy(),
            suggested_base
                .extension()
                .unwrap_or_else(|| std::ffi::OsStr::new("csv"))
                .to_string_lossy()
        );
        let selection = cx.prompt_for_new_path(&directory, Some(&name));
        self.confirming = true;
        cx.spawn_in(window, async move |this, cx| {
            let selection = selection.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.confirming = false;
                match selection {
                    Ok(Ok(Some(path))) => {
                        let res = this.controller.update(cx, |c, cx| {
                            c.start_filtered_export_active(this.window_id, target_doc_id, path, cx)
                        });
                        if let Err(error) = res {
                            this.set_message(error, cx);
                        }
                    }
                    Ok(Err(error)) => {
                        this.set_message(AppMessage::error(error.to_string()), cx);
                    }
                    _ => {}
                }
            });
        })
        .detach();
    }

    fn active_selected_column(&self, cx: &App) -> usize {
        self.snapshot(cx)
            .and_then(|i| i.selected_columns.first().copied())
            .or_else(|| {
                self.table
                    .read(cx)
                    .selected_cell()
                    .and_then(|(_, col)| self.table.read(cx).delegate().source_column_at(col, cx))
            })
            .unwrap_or(0)
    }

    fn active_selected_columns_or_current(&self, cx: &App) -> Vec<usize> {
        let mut cols = self
            .snapshot(cx)
            .map(|i| i.selected_columns)
            .unwrap_or_default();
        if cols.is_empty()
            && let Some((_, col)) = self.table.read(cx).selected_cell()
            && let Some(src) = self.table.read(cx).delegate().source_column_at(col, cx)
        {
            cols.push(src);
        }
        cols
    }

    fn restore_grid_focus_callback(
        &self,
        cx: &mut Context<Self>,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let weak = cx.weak_entity();
        move |window, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.editing = false;
                let table_focus = this
                    .snapshot(cx)
                    .is_some()
                    .then(|| this.table.focus_handle(cx));
                let root_focus = this.focus_handle.clone();
                window.defer(cx, move |window, cx| {
                    if let Some(tf) = table_focus {
                        tf.focus(window, cx);
                    } else {
                        root_focus.focus(window, cx);
                    }
                });
                cx.notify();
            });
        }
    }

    fn find_action(&mut self, _: &FindText, window: &mut Window, cx: &mut Context<Self>) {
        if self.snapshot(cx).is_some() && !self.confirming && !self.editing {
            self.find_bar
                .update(cx, |bar, cx| bar.open_find(false, window, cx));
        }
    }

    fn replace_action(&mut self, _: &ReplaceText, window: &mut Window, cx: &mut Context<Self>) {
        if self.snapshot(cx).is_some() && !self.confirming && !self.editing {
            self.find_bar
                .update(cx, |bar, cx| bar.open_find(true, window, cx));
        }
    }

    fn find_next_action(&mut self, _: &FindNext, window: &mut Window, cx: &mut Context<Self>) {
        if self.snapshot(cx).is_none() || self.confirming || self.editing {
            return;
        }
        if !self.find_bar.read(cx).is_open() {
            self.find_bar
                .update(cx, |bar, cx| bar.open_find(false, window, cx));
        } else {
            self.find_bar.update(cx, |bar, cx| bar.find_next(cx));
        }
    }

    fn find_previous_action(
        &mut self,
        _: &FindPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snapshot(cx).is_none() || self.confirming || self.editing {
            return;
        }
        if !self.find_bar.read(cx).is_open() {
            self.find_bar
                .update(cx, |bar, cx| bar.open_find(false, window, cx));
        } else {
            self.find_bar.update(cx, |bar, cx| bar.find_previous(cx));
        }
    }

    fn export_filtered_action(
        &mut self,
        _: &ExportFiltered,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.choose_export_filtered_destination(window, cx);
    }

    fn dialog_target(&self, doc_id: DocumentId) -> operations::DialogTarget {
        operations::DialogTarget {
            controller: self.controller.clone(),
            window_id: self.window_id,
            doc_id,
        }
    }

    fn can_open_schema_dialog(&mut self, info: &DocumentSnapshot, cx: &mut Context<Self>) -> bool {
        if info.structural_running {
            self.set_message(
                AppMessage::warning("Wait for the active file operation to finish."),
                cx,
            );
            return false;
        }
        if info.pending_split_confirm_bytes.is_some() {
            self.set_message(
                AppMessage::warning("Confirm or cancel the pending split first."),
                cx,
            );
            return false;
        }
        true
    }

    fn sort_rows_action(&mut self, _: &SortRows, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing || self.confirming {
            return;
        }
        let Some(info) = self.snapshot(cx) else {
            return;
        };
        if !self.can_open_schema_dialog(&info, cx) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let initial_col = self.active_selected_column(cx);
        let on_closed = self.restore_grid_focus_callback(cx);
        operations::open_sort_dialog(
            self.dialog_target(info.document_id),
            info.headers,
            initial_col,
            on_closed,
            window,
            cx,
        );
    }

    fn find_duplicates_action(
        &mut self,
        _: &FindDuplicates,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        let Some(info) = self.snapshot(cx) else {
            return;
        };
        if !self.can_open_schema_dialog(&info, cx) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let selected = info.selected_columns;
        let on_closed = self.restore_grid_focus_callback(cx);
        operations::open_duplicates_dialog(
            self.dialog_target(info.document_id),
            info.headers,
            selected,
            on_closed,
            window,
            cx,
        );
    }

    fn cleanup_columns_action(
        &mut self,
        _: &CleanupColumns,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        let Some(info) = self.snapshot(cx) else {
            return;
        };
        if !self.can_open_schema_dialog(&info, cx) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let selected = self.active_selected_columns_or_current(cx);
        let on_closed = self.restore_grid_focus_callback(cx);
        operations::open_cleanup_dialog(
            self.dialog_target(info.document_id),
            info.headers,
            selected,
            on_closed,
            window,
            cx,
        );
    }

    fn insert_rows_action(&mut self, _: &InsertRows, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing || self.confirming {
            return;
        }
        let Some(info) = self.snapshot(cx) else {
            return;
        };
        if !self.can_open_schema_dialog(&info, cx) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let default_pos = self
            .table
            .read(cx)
            .selected_cell()
            .map_or(1, |(row, _)| (row as u64).saturating_add(1));
        let on_closed = self.restore_grid_focus_callback(cx);
        operations::open_insert_dialog(
            self.dialog_target(info.document_id),
            true,
            default_pos,
            on_closed,
            window,
            cx,
        );
    }

    fn insert_columns_action(
        &mut self,
        _: &InsertColumns,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        let Some(info) = self.snapshot(cx) else {
            return;
        };
        if !self.can_open_schema_dialog(&info, cx) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let default_pos = (self.active_selected_column(cx) as u64).saturating_add(1);
        let on_closed = self.restore_grid_focus_callback(cx);
        operations::open_insert_dialog(
            self.dialog_target(info.document_id),
            false,
            default_pos,
            on_closed,
            window,
            cx,
        );
    }

    fn delete_selected_rows_action(
        &mut self,
        _: &DeleteSelectedRows,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        let res = self.controller.update(cx, |c, cx| {
            c.start_delete_selected_rows_active(self.window_id, cx)
        });
        if let Err(error) = res {
            self.set_message(error, cx);
        }
    }

    fn delete_selected_columns_action(
        &mut self,
        _: &DeleteSelectedColumns,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        let cols = self.active_selected_columns_or_current(cx);
        let res = self.controller.update(cx, |c, cx| {
            c.start_delete_selected_columns_active(self.window_id, cols, cx)
        });
        if let Err(error) = res {
            self.set_message(error, cx);
        }
    }

    fn move_columns_action(
        &mut self,
        _: &MoveColumns,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        let Some(info) = self.snapshot(cx) else {
            return;
        };
        if !self.can_open_schema_dialog(&info, cx) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let selected = self.active_selected_columns_or_current(cx);
        let on_closed = self.restore_grid_focus_callback(cx);
        operations::open_move_columns_dialog(
            self.dialog_target(info.document_id),
            info.headers,
            selected,
            on_closed,
            window,
            cx,
        );
    }

    fn split_column_action(
        &mut self,
        _: &SplitColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        let Some(info) = self.snapshot(cx) else {
            return;
        };
        if !self.can_open_schema_dialog(&info, cx) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let initial_col = self.active_selected_column(cx);
        let on_closed = self.restore_grid_focus_callback(cx);
        operations::open_split_column_dialog(
            self.dialog_target(info.document_id),
            info.headers,
            initial_col,
            on_closed,
            window,
            cx,
        );
    }

    fn combine_columns_action(
        &mut self,
        _: &CombineColumns,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        let Some(info) = self.snapshot(cx) else {
            return;
        };
        if !self.can_open_schema_dialog(&info, cx) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let selected = self.active_selected_columns_or_current(cx);
        let on_closed = self.restore_grid_focus_callback(cx);
        operations::open_combine_columns_dialog(
            self.dialog_target(info.document_id),
            info.headers,
            selected,
            on_closed,
            window,
            cx,
        );
    }

    fn open_output_path_callback(
        &self,
        cx: &mut Context<Self>,
    ) -> impl Fn(PathBuf, &mut Window, &mut App) + 'static {
        let weak = cx.weak_entity();
        move |path, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.open_paths_direct(vec![path], OpenOptions::default(), None, window, cx);
            });
        }
    }

    fn workflow_dialog_closed_callback(
        &self,
        kind: WorkflowDialogKind,
        cx: &mut Context<Self>,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let controller = self.controller.clone();
        let window_id = self.window_id;
        let restore = self.restore_grid_focus_callback(cx);
        move |window, cx| {
            controller.update(cx, |c, _| c.release_workflow_dialog(kind, window_id));
            restore(window, cx);
        }
    }

    fn open_combine_dialog_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing || self.confirming {
            return;
        }
        if !self.controller.update(cx, |c, cx| {
            c.claim_workflow_dialog(WorkflowDialogKind::Combine, self.window_id, cx)
        }) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let on_open = self.open_output_path_callback(cx);
        let on_closed = self.workflow_dialog_closed_callback(WorkflowDialogKind::Combine, cx);
        workflows_ui::open_combine_dialog(self.controller.clone(), on_open, on_closed, window, cx);
    }

    fn open_join_dialog_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing || self.confirming {
            return;
        }
        if !self.controller.update(cx, |c, cx| {
            c.claim_workflow_dialog(WorkflowDialogKind::Join, self.window_id, cx)
        }) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let on_open = self.open_output_path_callback(cx);
        let on_closed = self.workflow_dialog_closed_callback(WorkflowDialogKind::Join, cx);
        workflows_ui::open_join_dialog(self.controller.clone(), on_open, on_closed, window, cx);
    }

    fn open_inspect_dialog_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_inspect_dialog_for_target(None, window, cx);
    }

    fn open_inspect_dialog_for_target(
        &mut self,
        failed_target: Option<Box<FailedOpen>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        if !self.controller.update(cx, |c, cx| {
            c.claim_workflow_dialog(WorkflowDialogKind::Inspect, self.window_id, cx)
        }) {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        self.controller.update(cx, |c, cx| {
            if let Some(failed) = failed_target {
                c.inspect_configure_failed_open(&failed, cx);
            } else {
                c.inspect_configure_for_active(self.window_id, cx);
            }
        });
        let on_open = self.open_output_path_callback(cx);
        let on_closed = self.workflow_dialog_closed_callback(WorkflowDialogKind::Inspect, cx);
        workflows_ui::open_inspect_dialog(
            self.controller.clone(),
            self.window_id,
            on_open,
            on_closed,
            window,
            cx,
        );
    }

    fn combine_files_action(
        &mut self,
        _: &CombineFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_combine_dialog_now(window, cx);
    }

    fn join_files_action(&mut self, _: &JoinFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.open_join_dialog_now(window, cx);
    }

    fn inspect_file_action(
        &mut self,
        _: &InspectFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_inspect_dialog_now(window, cx);
    }

    fn temporary_storage_action(
        &mut self,
        _: &TemporaryStorage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let on_closed = self.restore_grid_focus_callback(cx);
        workflows_ui::open_temporary_storage_dialog(self.controller.clone(), on_closed, window, cx);
    }

    fn set_theme_preference(&mut self, preference: ThemePreference, cx: &mut Context<Self>) {
        if let Ok(resolved) = self
            .controller
            .update(cx, |c, cx| c.set_theme_preference(preference, cx))
        {
            apply_resolved_theme(&resolved, cx);
            cx.refresh_windows();
        }
    }

    #[inline(never)]
    fn render_theme_banner(
        &self,
        tokens: &quarry_workbench::theme::Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let text = self.controller.read(cx).theme_warning()?.to_owned();
        let can_dismiss = self.controller.read(cx).resolved_theme().warning.is_none();
        Some(
            div()
                .id("theme-warning-banner")
                .role(Role::Status)
                .aria_label(text.clone())
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .border_t_1()
                .border_color(cx.theme().border)
                .bg(rgb(tokens.warning_fill.to_rgb_hex()))
                .text_color(rgb(tokens.warning_text.to_rgb_hex()))
                .child(div().flex_1().min_w_0().child(text))
                .when(can_dismiss, |bar| {
                    bar.child(
                        Button::new("dismiss-theme-warning")
                            .label("×")
                            .accessibility_label("Dismiss theme notice")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |c, cx| c.clear_theme_error(cx));
                            })),
                    )
                })
                .into_any_element(),
        )
    }

    fn theme_system_action(&mut self, _: &ThemeSystem, _: &mut Window, cx: &mut Context<Self>) {
        self.set_theme_preference(ThemePreference::System, cx);
    }

    fn theme_light_action(&mut self, _: &ThemeLight, _: &mut Window, cx: &mut Context<Self>) {
        self.set_theme_preference(ThemePreference::Light, cx);
    }

    fn theme_dark_action(&mut self, _: &ThemeDark, _: &mut Window, cx: &mut Context<Self>) {
        self.set_theme_preference(ThemePreference::Dark, cx);
    }

    fn theme_omarchy_action(&mut self, _: &ThemeOmarchy, _: &mut Window, cx: &mut Context<Self>) {
        self.set_theme_preference(ThemePreference::Omarchy, cx);
    }

    fn about_quarry_action(
        &mut self,
        _: &AboutQuarry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing || self.confirming {
            return;
        }
        self.editing = true;
        self.panels.update(cx, |p, cx| p.close(window, cx));
        let on_closed = self.restore_grid_focus_callback(cx);
        workflows_ui::open_about_dialog(on_closed, window, cx);
    }
    #[inline(never)]
    fn render_gzip_banner(
        &self,
        snap: &DocumentSnapshot,
        tokens: &quarry_workbench::theme::Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if snap.compressed_pending {
            Some(
                div()
                    .id("gzip-pending-banner")
                    .role(Role::Alert)
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .bg(rgb(tokens.warning_fill.to_rgb_hex()))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().min_w_0().child(
                        "Showing initial preview rows of this compressed file. Decompress the full file into temporary storage to inspect all rows, search, filter, or save an uncompressed copy.",
                    ))
                    .child(
                        Button::new("gzip-start-decompress")
                            .label("Decompress Full File")
                            .primary()
                            .small()
                            .flex_shrink_0()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let res = this.controller.update(cx, |c, cx| {
                                    c.start_pending_decompression_active(this.window_id, cx)
                                });
                                if let Err(error) = res {
                                    this.set_message(AppMessage::warning(error), cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("gzip-preview-only")
                            .label("Preview Only")
                            .small()
                            .flex_shrink_0()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let res = this.controller.update(cx, |c, cx| {
                                    c.stay_in_compressed_preview_active(this.window_id, cx)
                                });
                                if let Err(error) = res {
                                    this.set_message(AppMessage::warning(error), cx);
                                }
                            })),
                    )
                    .into_any_element(),
            )
        } else if let Some(ref decomp) = snap.decompression_status {
            Some(
                div()
                    .id("gzip-decompressing-banner")
                    .role(Role::Status)
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .bg(cx.theme().secondary)
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(decomp.label.clone()),
                    )
                    .when(decomp.cancel_enabled, |bar| {
                        bar.child(
                            Button::new("gzip-cancel-decompress")
                                .label("Cancel")
                                .danger()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.controller.update(cx, |c, cx| {
                                        c.cancel_decompression_active(this.window_id, cx);
                                    });
                                })),
                        )
                    })
                    .into_any_element(),
            )
        } else if snap.compressed_preview {
            let unreadable = snap.compressed_unreadable_text;
            let preview_msg = if unreadable {
                "Showing initial preview rows only because later rows contain unreadable delimited text."
            } else {
                "Showing initial preview rows only (read-only). Reload from Disk to decompress the full file."
            };
            Some(
                div()
                    .id("gzip-preview-banner")
                    .role(Role::Status)
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .bg(cx.theme().secondary)
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(preview_msg),
                    )
                    .when(unreadable, |bar| {
                        bar.child(
                            Button::new("gzip-preview-inspect")
                                .label("Inspect File…")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_inspect_dialog_now(window, cx);
                                })),
                        )
                    })
                    .when(!unreadable, |bar| {
                        bar.child(
                            Button::new("gzip-preview-reload")
                                .label("Reload from Disk")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.reload_action(&ReloadFromDisk, window, cx);
                                })),
                        )
                    })
                    .into_any_element(),
            )
        } else {
            None
        }
    }

    #[inline(never)]
    fn render_operation_banner(
        &self,
        snap: &DocumentSnapshot,
        tokens: &quarry_workbench::theme::Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Some(preview) = &snap.duplicate_preview {
            Some(
                div()
                    .id("duplicate-preview-banner")
                    .role(Role::Alert)
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .bg(rgb(tokens.warning_fill.to_rgb_hex()))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().child(format!(
                        "Found {} extra duplicate row{} ({} unique of {} total).",
                        preview.duplicate_rows,
                        if preview.duplicate_rows == 1 { "" } else { "s" },
                        preview.unique_rows,
                        preview.total_rows
                    )))
                    .child(
                        Button::new("confirm-remove-duplicates")
                            .label("Remove Duplicates")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let res = this.controller.update(cx, |c, cx| {
                                    c.confirm_duplicate_removal_active(this.window_id, cx)
                                });
                                match res {
                                    Ok(notice) => {
                                        this.set_message(AppMessage::status(notice), cx);
                                    }
                                    Err(error) => this.set_message(error, cx),
                                }
                            })),
                    )
                    .child(
                        Button::new("cancel-duplicate-preview")
                            .label("Cancel")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |c, cx| {
                                    c.cancel_structural_edit_active(this.window_id, cx);
                                });
                            })),
                    )
                    .into_any_element(),
            )
        } else if let Some(bytes) = snap.pending_split_confirm_bytes {
            Some(
                div()
                    .id("split-confirm-banner")
                    .role(Role::Alert)
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .bg(rgb(tokens.warning_fill.to_rgb_hex()))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().child(format!(
                        "Split will create a temporary working copy (~{}). Continue?",
                        format_bytes(bytes)
                    )))
                    .child(
                        Button::new("confirm-split-materialization")
                            .label("Continue Split")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let res = this.controller.update(cx, |c, cx| {
                                    c.confirm_pending_materialization_active(this.window_id, cx)
                                });
                                if let Err(error) = res {
                                    this.set_message(error, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("cancel-split-materialization")
                            .label("Cancel")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |c, cx| {
                                    c.cancel_pending_materialization_active(this.window_id, cx);
                                });
                            })),
                    )
                    .into_any_element(),
            )
        } else if snap.structural_running || snap.export_running || snap.filter_running {
            let op_label = if snap.structural_running {
                let base = snap
                    .structural_status
                    .clone()
                    .unwrap_or_else(|| "Applying operation…".into());
                match snap.structural_progress {
                    Some(frac) => format!("{base} ({:.0}%)", (frac * 100.0).clamp(0.0, 100.0)),
                    None => base,
                }
            } else if snap.export_running {
                snap.export_status
                    .clone()
                    .unwrap_or_else(|| "Exporting filtered rows…".into())
            } else {
                snap.filter_status
                    .clone()
                    .unwrap_or_else(|| "Filtering rows…".into())
            };
            let is_structural = snap.structural_running;
            Some(
                div()
                    .id("operation-progress-banner")
                    .role(Role::Status)
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .bg(cx.theme().secondary)
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(op_label),
                    )
                    .child(
                        Button::new("cancel-operation")
                            .label("Cancel")
                            .small()
                            .danger()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.controller.update(cx, |c, cx| {
                                    if is_structural {
                                        c.cancel_structural_edit_active(this.window_id, cx);
                                    } else {
                                        c.cancel_filter_or_export_active(this.window_id, cx);
                                    }
                                });
                            })),
                    )
                    .into_any_element(),
            )
        } else {
            None
        }
    }
}

impl Render for QuarryWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.focused(cx).is_none() {
            self.focus_handle.focus(window, cx);
        }
        let info = self.snapshot(cx);
        let loaded = info.is_some();
        let tabs = self.controller.read(cx).tabs_for_window(self.window_id);
        let total_open_tabs = self.controller.read(cx).scheduler().total_tabs();
        let show_whitespace = self.controller.read(cx).show_all_whitespace();
        let filename = info
            .as_ref()
            .map(|i| i.filename.clone())
            .unwrap_or_else(|| "Open file…".into());
        let can_undo = self.command_enabled(CommandId::Undo, cx);
        let can_redo = self.command_enabled(CommandId::Redo, cx);
        let can_save = self.command_enabled(CommandId::Save, cx);
        let can_save_as = self.command_enabled(CommandId::SaveAs, cx);
        let busy = info.as_ref().is_some_and(|i| i.busy);
        let format = self
            .panels
            .update(cx, |panels, cx| panels.render_format(window, cx));
        let columns = self
            .panels
            .update(cx, |panels, cx| panels.render_columns(window, cx));
        let filters = self
            .panels
            .update(cx, |panels, cx| panels.render_filters(window, cx));
        let tokens = self.controller.read(cx).active_palette();
        let message = self
            .controller
            .read(cx)
            .window_message(self.window_id)
            .cloned();

        let mut body = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_size(px(13.))
            .track_focus(&self.focus_handle)
            .key_context(QUARRY_KEY_CONTEXT)
            .on_action(cx.listener(Self::new_window_action))
            .on_action(cx.listener(Self::open_action))
            .on_action(cx.listener(Self::open_utf8))
            .on_action(cx.listener(Self::open_utf16le))
            .on_action(cx.listener(Self::open_utf16be))
            .on_action(cx.listener(Self::open_win1252))
            .on_action(cx.listener(Self::open_iso88591))
            .on_action(cx.listener(Self::close_tab_action))
            .on_action(cx.listener(Self::close_window_action))
            .on_action(cx.listener(Self::move_tab_to_new_window_action))
            .on_action(cx.listener(Self::next_tab_action))
            .on_action(cx.listener(Self::previous_tab_action))
            .on_action(cx.listener(Self::reload_action))
            .on_action(cx.listener(Self::save_action))
            .on_action(cx.listener(Self::save_as_action))
            .on_action(cx.listener(Self::save_as_utf8_action))
            .on_action(cx.listener(Self::export_filtered_action))
            .on_action(cx.listener(Self::discard_action))
            .on_action(cx.listener(Self::undo_action))
            .on_action(cx.listener(Self::redo_action))
            .on_action(cx.listener(Self::copy_action))
            .on_action(cx.listener(Self::edit_action))
            .on_action(cx.listener(Self::rename_header_action))
            .on_action(cx.listener(Self::find_action))
            .on_action(cx.listener(Self::find_next_action))
            .on_action(cx.listener(Self::find_previous_action))
            .on_action(cx.listener(Self::replace_action))
            .on_action(cx.listener(Self::sort_rows_action))
            .on_action(cx.listener(Self::find_duplicates_action))
            .on_action(cx.listener(Self::cleanup_columns_action))
            .on_action(cx.listener(Self::insert_rows_action))
            .on_action(cx.listener(Self::insert_columns_action))
            .on_action(cx.listener(Self::delete_selected_rows_action))
            .on_action(cx.listener(Self::delete_selected_columns_action))
            .on_action(cx.listener(Self::move_columns_action))
            .on_action(cx.listener(Self::split_column_action))
            .on_action(cx.listener(Self::combine_columns_action))
            .on_action(cx.listener(Self::toggle_whitespace_action))
            .on_action(cx.listener(Self::theme_system_action))
            .on_action(cx.listener(Self::theme_light_action))
            .on_action(cx.listener(Self::theme_dark_action))
            .on_action(cx.listener(Self::theme_omarchy_action))
            .on_action(cx.listener(Self::toggle_columns_action))
            .on_action(cx.listener(Self::toggle_filters_action))
            .on_action(cx.listener(Self::combine_files_action))
            .on_action(cx.listener(Self::join_files_action))
            .on_action(cx.listener(Self::inspect_file_action))
            .on_action(cx.listener(Self::temporary_storage_action))
            .on_action(cx.listener(Self::about_quarry_action))
            .on_action(cx.listener(Self::quit_action));

        #[cfg(not(target_os = "macos"))]
        {
            body = body.child(
                div()
                    .h(px(28.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .bg(rgb(tokens.chrome.to_rgb_hex()))
                    .child(self.menu_bar.clone()),
            );
        }

        if !tabs.is_empty() {
            let tab_bar = div()
                .id("window-tab-bar")
                .role(Role::TabList)
                .aria_label("Open documents")
                .h(px(32.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .bg(rgb(tokens.chrome.to_rgb_hex()))
                .child(
                    div()
                        .id("window-tabs-scroll")
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .overflow_x_scroll()
                        .children(tabs.iter().map(|entry| {
                            let doc_id = entry.id;
                            let is_dormant = !matches!(entry.residency, ResidencyState::Hydrated);
                            let label =
                                format!("{}{}", entry.title, if entry.dirty { " •" } else { "" });
                            div()
                                .id(("tab-item", doc_id.raw()))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .gap_1()
                                .px_2()
                                .h(px(24.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(if entry.active {
                                    rgb(tokens.selection_strong.to_rgb_hex())
                                } else {
                                    rgb(tokens.separator.to_rgb_hex())
                                })
                                .bg(if entry.active {
                                    rgb(tokens.canvas.to_rgb_hex())
                                } else {
                                    rgb(tokens.chrome.to_rgb_hex())
                                })
                                .child(
                                    Button::new(("select-tab", doc_id.raw()))
                                        .label(label)
                                        .ghost()
                                        .xsmall()
                                        .text_color(if is_dormant && !entry.active {
                                            rgb(tokens.text_secondary.to_rgb_hex())
                                        } else {
                                            rgb(tokens.text.to_rgb_hex())
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            let res =
                                                this.controller.update(cx, |controller, cx| {
                                                    controller.activate_tab(
                                                        this.window_id,
                                                        doc_id,
                                                        cx,
                                                    )
                                                });
                                            if let Err(error) = res {
                                                this.set_message(AppMessage::warning(error), cx);
                                            } else {
                                                this.sync_from_controller(window, cx);
                                            }
                                        })),
                                )
                                .child(
                                    Button::new(("close-tab", doc_id.raw()))
                                        .label("×")
                                        .accessibility_label(format!("Close {}", entry.title))
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.close_tab_by_id(doc_id, true, window, cx);
                                        })),
                                )
                        })),
                )
                .child(
                    Button::new("move-tab-new-window")
                        .label("Move to Window")
                        .ghost()
                        .xsmall()
                        .disabled(!loaded)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.move_tab_to_new_window_action(&MoveTabToNewWindow, window, cx);
                        })),
                )
                .child(
                    Button::new("new-window-btn")
                        .label("+ Window")
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_window_action(&NewWindow, window, cx);
                        })),
                );
            body = body.child(tab_bar);
        }

        body = body.child(
            div()
                .id("document-toolbar")
                .role(Role::Toolbar)
                .aria_label("Document toolbar")
                .h(px(42.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .bg(rgb(tokens.chrome.to_rgb_hex()))
                .child(
                    Button::new("open-file")
                        .label(filename.clone())
                        .ghost()
                        .small()
                        .max_w(px(220.))
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.choose_open_with_encoding(None, window, cx)
                        })),
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
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.undo_action(&UndoEdit, window, cx);
                        })),
                )
                .child(
                    Button::new("redo")
                        .label("Redo")
                        .ghost()
                        .small()
                        .disabled(!can_redo)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.redo_action(&RedoEdit, window, cx);
                        })),
                )
                .child(columns)
                .child(filters)
                .child(
                    Button::new("toolbar-find")
                        .label("Find")
                        .ghost()
                        .small()
                        .disabled(!loaded)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.find_action(&FindText, window, cx);
                        })),
                )
                .child(
                    Button::new("toolbar-sort")
                        .label("Sort")
                        .ghost()
                        .small()
                        .disabled(!loaded || busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.sort_rows_action(&SortRows, window, cx);
                        })),
                )
                .child(
                    Button::new("toolbar-duplicates")
                        .label("Duplicates")
                        .ghost()
                        .small()
                        .disabled(!loaded || busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.find_duplicates_action(&FindDuplicates, window, cx);
                        })),
                )
                .child(
                    Button::new("toolbar-cleanup")
                        .label("Clean")
                        .ghost()
                        .small()
                        .disabled(!loaded || busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cleanup_columns_action(&CleanupColumns, window, cx);
                        })),
                )
                .child(
                    Button::new("toggle-whitespace")
                        .label(if show_whitespace {
                            "Whitespace: On"
                        } else {
                            "Whitespace"
                        })
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_whitespace_action(&ShowAllWhitespace, window, cx);
                        })),
                )
                .child(div().flex_1())
                .child(
                    Button::new("save-file")
                        .label("Save")
                        .ghost()
                        .small()
                        .disabled(!can_save)
                        .on_click(cx.listener(|this, _, _, cx| this.save_in_place(cx))),
                )
                .child(
                    Button::new("save-copy")
                        .label("Save As…")
                        .ghost()
                        .small()
                        .disabled(!can_save_as)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.choose_save_destination(None, window, cx);
                        })),
                ),
        );

        if loaded && self.find_bar.read(cx).is_open() {
            body = body.child(self.find_bar.clone());
        }

        if let Some(ref snap) = info {
            if let Some(banner) = self.render_gzip_banner(snap, &tokens, cx) {
                body = body.child(banner);
            }
            if let Some(banner) = self.render_operation_banner(snap, &tokens, cx) {
                body = body.child(banner);
            }
        }

        if loaded {
            let table = self.table.clone();
            let shift_horizontal_scroll = canvas(
                move |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |_, hitbox, window, _| {
                    let table = table.clone();
                    let line_height = window.line_height();
                    window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                        if !(phase.capture()
                            && hitbox.should_handle_scroll(window)
                            && event.modifiers.shift)
                        {
                            return;
                        }
                        let delta = event.delta.pixel_delta(line_height);
                        if delta.x.is_zero() && !delta.y.is_zero() {
                            table.update(cx, |table, cx| {
                                let mut offset = table.horizontal_scroll_handle.offset();
                                offset.x += delta.y;
                                table.horizontal_scroll_handle.set_offset(offset);
                                cx.notify();
                            });
                            cx.stop_propagation();
                        }
                    });
                },
            )
            .absolute()
            .size_full();
            body = body.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(shift_horizontal_scroll)
                    .child(
                        DataTable::new(&self.table)
                            .accessibility_label(format!("{filename} data grid"))
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
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("open-empty")
                                    .label("Open file…")
                                    .primary()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.choose_open_with_encoding(None, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("welcome-combine")
                                    .label("Combine Files…")
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_combine_dialog_now(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("welcome-join")
                                    .label("Join Files…")
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_join_dialog_now(window, cx);
                                    })),
                            ),
                    ),
            );
        }

        if let Some(theme_banner) = self.render_theme_banner(&tokens, cx) {
            body = body.child(theme_banner);
        }

        if let Some(msg) = message {
            let is_error = msg.severity == MessageSeverity::Error;
            let is_warning = msg.severity == MessageSeverity::Warning;
            let inspect_target = msg.inspection_target.clone();
            let can_inspect_banner = inspect_target.is_some()
                || ((is_error || is_warning)
                    && (msg.diagnostic.is_some()
                        || info.as_ref().is_some_and(|i| i.has_diagnostic)));
            body = body.child(
                div()
                    .id("status-banner")
                    .role(if is_error || is_warning {
                        Role::Alert
                    } else {
                        Role::Status
                    })
                    .aria_label(msg.text.clone())
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .bg(if is_error {
                        rgb(tokens.error_fill.to_rgb_hex())
                    } else if is_warning {
                        rgb(tokens.warning_fill.to_rgb_hex())
                    } else {
                        rgb(tokens.chrome.to_rgb_hex())
                    })
                    .text_color(if is_error {
                        rgb(tokens.error_text.to_rgb_hex()).into()
                    } else if is_warning {
                        rgb(tokens.warning_text.to_rgb_hex()).into()
                    } else {
                        cx.theme().foreground
                    })
                    .child(div().flex_1().min_w_0().child(msg.text))
                    .when(can_inspect_banner, |bar| {
                        let target = inspect_target.clone();
                        bar.child(
                            Button::new("banner-inspect")
                                .label("Inspect…")
                                .small()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_inspect_dialog_for_target(target.clone(), window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("dismiss-message")
                            .label("×")
                            .accessibility_label("Dismiss message")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.controller.update(cx, |controller, cx| {
                                    controller.set_window_message(this.window_id, None, cx);
                                });
                            })),
                    ),
            );
        }

        let status = info
            .as_ref()
            .map(|info| {
                let rows = if info.filter_active {
                    format!(
                        "{} of {} rows",
                        number(info.available_rows),
                        number(info.total_rows)
                    )
                } else {
                    format!("{} rows", number(info.total_rows))
                };
                let compressed_suffix = info
                    .compressed_label
                    .as_ref()
                    .map(|l| format!("   ·   {l}"))
                    .unwrap_or_default();
                let join_suffix = if info.join_reserved {
                    "   ·   Reserved by Join"
                } else {
                    ""
                };
                format!(
                    "{}   ·   {:.2} MiB   ·   {} of {} columns   ·   {}{}{}{}",
                    rows,
                    info.file_size as f64 / 1_048_576.,
                    info.shown_columns.len(),
                    info.headers.len(),
                    encoding_label(info.encoding),
                    compressed_suffix,
                    join_suffix,
                    if info.dirty {
                        "   ·   Unsaved edits"
                    } else {
                        ""
                    }
                )
            })
            .unwrap_or_else(|| "Ready".into());

        let progress = if let Some(info) = &info {
            if let Some(ref decomp) = info.decompression_status {
                decomp.label.clone()
            } else if info.saving {
                "Saving…".into()
            } else if info.filter_active {
                if info.filter_done {
                    "Filter complete".into()
                } else {
                    "Filtering…".into()
                }
            } else if info.index_done {
                "Index complete".into()
            } else {
                format!("Indexing {:.0}%", info.index_fraction * 100.0)
            }
        } else {
            #[cfg(target_os = "macos")]
            let hint = "⌘O to open · ⌘N new window";
            #[cfg(not(target_os = "macos"))]
            let hint = "Ctrl+O to open · Ctrl+N new window";
            hint.into()
        };

        let tabs_badge = if total_open_tabs > 1 {
            format!(
                "{} tabs ({} active)",
                total_open_tabs,
                self.controller.read(cx).scheduler().hydrated_count()
            )
        } else {
            "Quarry GPUI".into()
        };

        body.child(
            div()
                .id("document-status")
                .role(Role::Status)
                .aria_label(format!("{status} · {progress}"))
                .flex()
                .items_center()
                .h(px(30.))
                .flex_shrink_0()
                .px_3()
                .gap_4()
                .border_t_1()
                .border_color(cx.theme().border)
                .bg(rgb(tokens.chrome.to_rgb_hex()))
                .text_size(px(12.))
                .font_features(grid::tabular_font_features())
                .text_color(cx.theme().muted_foreground)
                .child(status)
                .child(div().flex_1())
                .child(progress)
                .child(div().child(tabs_badge)),
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

fn window_appearance_to_theme(appearance: WindowAppearance) -> ThemeAppearance {
    match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeAppearance::Dark,
        WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeAppearance::Light,
    }
}

pub(crate) fn apply_resolved_theme(resolved: &ResolvedTheme, cx: &mut App) {
    let mode = match resolved.appearance {
        ThemeAppearance::Light => ThemeMode::Light,
        ThemeAppearance::Dark => ThemeMode::Dark,
    };
    Theme::change(mode, None, cx);
    let tokens = &resolved.palette;
    let is_dark = resolved.appearance == ThemeAppearance::Dark;
    let contrast_safe_primary_variant =
        |preferred: quarry_workbench::Rgba, alternate: quarry_workbench::Rgba| {
            if quarry_workbench::contrast_ratio(tokens.on_accent, preferred) >= 4.5 {
                preferred
            } else if quarry_workbench::contrast_ratio(tokens.on_accent, alternate) >= 4.5 {
                alternate
            } else {
                tokens.selection_strong
            }
        };
    let primary_hover = contrast_safe_primary_variant(
        tokens.selection_strong.blend(
            if is_dark {
                tokens.text
            } else {
                tokens.on_accent
            },
            0.10,
        ),
        tokens.selection_strong.blend(tokens.canvas, 0.10),
    );
    let primary_active = contrast_safe_primary_variant(
        tokens.selection_strong.blend(
            if is_dark {
                tokens.canvas
            } else {
                tokens.on_accent
            },
            0.18,
        ),
        tokens
            .selection_strong
            .blend(if is_dark { tokens.text } else { tokens.canvas }, 0.16),
    );

    let theme = Theme::global_mut(cx);
    theme.font_size = px(quarry_workbench::theme::GRID_FONT_SIZE);
    theme.radius = px(f32::from(quarry_workbench::theme::CONTROL_RADIUS));
    theme.radius_lg = px(f32::from(quarry_workbench::theme::WINDOW_RADIUS));
    theme.colors.background = rgb(tokens.chrome.to_rgb_hex()).into();
    theme.colors.foreground = rgb(tokens.text.to_rgb_hex()).into();
    theme.colors.border = rgb(tokens.separator.to_rgb_hex()).into();
    theme.colors.input = rgb(tokens.control_border.to_rgb_hex()).into();
    theme.colors.ring = rgb(tokens.selection_strong.to_rgb_hex()).into();
    theme.colors.muted = rgb(tokens.hover.to_rgb_hex()).into();
    theme.colors.muted_foreground = rgb(tokens.text_secondary.to_rgb_hex()).into();
    theme.colors.accent = rgb(tokens.hover.to_rgb_hex()).into();
    theme.colors.accent_foreground = rgb(tokens.text.to_rgb_hex()).into();
    theme.colors.secondary = rgb(tokens.control.to_rgb_hex()).into();
    theme.colors.secondary_hover = rgb(tokens.hover.to_rgb_hex()).into();
    theme.colors.secondary_active = rgb(tokens.pressed.to_rgb_hex()).into();
    theme.colors.secondary_foreground = rgb(tokens.text.to_rgb_hex()).into();
    theme.colors.button = rgb(tokens.control.to_rgb_hex()).into();
    theme.colors.button_hover = rgb(tokens.hover.to_rgb_hex()).into();
    theme.colors.button_active = rgb(tokens.pressed.to_rgb_hex()).into();
    theme.colors.button_foreground = rgb(tokens.text.to_rgb_hex()).into();
    theme.colors.primary = rgb(tokens.selection_strong.to_rgb_hex()).into();
    theme.colors.primary_hover = rgb(primary_hover.to_rgb_hex()).into();
    theme.colors.primary_active = rgb(primary_active.to_rgb_hex()).into();
    theme.colors.primary_foreground = rgb(tokens.on_accent.to_rgb_hex()).into();
    theme.colors.button_primary = theme.colors.primary;
    theme.colors.button_primary_hover = theme.colors.primary_hover;
    theme.colors.button_primary_active = theme.colors.primary_active;
    theme.colors.button_primary_foreground = theme.colors.primary_foreground;
    theme.colors.table = rgb(tokens.canvas.to_rgb_hex()).into();
    theme.colors.table_even = rgb(tokens.zebra.to_rgb_hex()).into();
    theme.colors.table_head = rgb(tokens.header_fill.to_rgb_hex()).into();
    theme.colors.table_head_foreground = rgb(tokens.text_secondary.to_rgb_hex()).into();
    theme.colors.table_row_border = rgb(tokens.grid_line.to_rgb_hex()).into();
    theme.colors.table_hover = rgb(tokens.hover.to_rgb_hex()).into();
    theme.colors.table_active = rgb(tokens.selection.to_rgb_hex()).into();
    theme.colors.table_active_border = rgb(tokens.selection_strong.to_rgb_hex()).into();
    theme.colors.list = rgb(tokens.canvas.to_rgb_hex()).into();
    theme.colors.list_hover = rgb(tokens.hover.to_rgb_hex()).into();
    theme.colors.list_active = rgb(tokens.selection.to_rgb_hex()).into();
    theme.colors.list_active_border = rgb(tokens.selection_strong.to_rgb_hex()).into();
    theme.colors.popover = rgb(tokens.canvas.to_rgb_hex()).into();
    theme.colors.popover_foreground = rgb(tokens.text.to_rgb_hex()).into();
    theme.colors.link = rgb(tokens.accent_text.to_rgb_hex()).into();
    theme.colors.link_hover = rgb(tokens.accent_text.to_rgb_hex()).into();
    theme.colors.link_active = rgb(tokens.accent_text.to_rgb_hex()).into();
    theme.colors.danger = rgb(tokens.error_text.to_rgb_hex()).into();
    theme.colors.danger_foreground = rgb(tokens.canvas.to_rgb_hex()).into();
    theme.colors.button_danger = rgb(tokens.error_fill.to_rgb_hex()).into();
    theme.colors.button_danger_hover = rgb(tokens
        .error_fill
        .blend(tokens.error_text, 0.14)
        .to_rgb_hex())
    .into();
    theme.colors.button_danger_active = rgb(tokens
        .error_fill
        .blend(tokens.error_text, 0.24)
        .to_rgb_hex())
    .into();
    theme.colors.button_danger_foreground = rgb(tokens.error_text.to_rgb_hex()).into();
    theme.colors.warning = rgb(tokens.warning_text.to_rgb_hex()).into();
    theme.colors.warning_foreground = rgb(tokens.canvas.to_rgb_hex()).into();
    theme.colors.button_warning = rgb(tokens.warning_fill.to_rgb_hex()).into();
    theme.colors.button_warning_hover = rgb(tokens
        .warning_fill
        .blend(tokens.warning_text, 0.14)
        .to_rgb_hex())
    .into();
    theme.colors.button_warning_active = rgb(tokens
        .warning_fill
        .blend(tokens.warning_text, 0.24)
        .to_rgb_hex())
    .into();
    theme.colors.button_warning_foreground = rgb(tokens.warning_text.to_rgb_hex()).into();
    theme.colors.success = rgb(tokens.success_text.to_rgb_hex()).into();
    theme.colors.success_foreground = rgb(tokens.canvas.to_rgb_hex()).into();
    theme.tokens = theme.colors.into();
    Theme::sync_base(cx);
}

#[cfg(test)]
fn apply_light_theme(cx: &mut App) {
    let light =
        quarry_workbench::resolve_theme(ThemePreference::Light, ThemeAppearance::Light, None);
    apply_resolved_theme(&light, cx);
}

fn open_quarry_window(
    controller: Entity<WorkbenchController>,
    initial_paths: Vec<PathBuf>,
    moved_tab: Option<DocumentId>,
    cx: &mut App,
) {
    let window_id = controller.update(cx, |controller, _| controller.allocate_window());
    let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
    let controller_for_handle = controller.clone();
    if let Ok(handle) = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(860.), px(500.))),
            titlebar: Some(TitlebarOptions {
                title: Some("Quarry".into()),
                ..Default::default()
            }),
            app_id: Some("dev.quarry.gpui".into()),
            ..Default::default()
        },
        move |window, cx| {
            let view = cx.new(|cx| QuarryWindow::new(window_id, controller.clone(), window, cx));
            if let Some(doc_id) = moved_tab {
                let _ = controller.update(cx, |c, cx| c.move_tab_to_window(doc_id, window_id, cx));
                view.update(cx, |this, cx| this.sync_from_controller(window, cx));
            } else if !initial_paths.is_empty() {
                view.update(cx, |this, cx| {
                    this.open_paths_direct(initial_paths, OpenOptions::default(), None, window, cx);
                });
            }
            cx.new(|cx| Root::new(view, window, cx))
        },
    ) {
        controller_for_handle.update(cx, |controller, _| {
            controller.register_window_handle(window_id, handle);
        });
    }
}

fn main() {
    let initial_paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            bind_shortcuts(cx);
            install_menus(cx);
            let controller = WorkbenchController::init(cx);
            let initial_theme = controller.read(cx).resolved_theme().clone();
            apply_resolved_theme(&initial_theme, cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.spawn(async move |cx| {
                cx.update(|cx| {
                    open_quarry_window(controller, initial_paths, None, cx);
                });
            })
            .detach();
            cx.activate(true);
        });
}
