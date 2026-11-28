//! Fuzzy Command Palette (`Cmd/Ctrl+K`) and Go to Row / Column (`Cmd/Ctrl+L` / `#`) overlay.
//!
//! Captures the invoking `(WindowId, Option<DocumentId>)` when opened, routes all command
//! execution through `quarry_workbench::CommandId` + `evaluate_command`, caches displayed rows
//! across selection movement, scrolls the selected result into view, and exposes list/item
//! accessibility metadata.

use gpui_kit::component::{
    ActiveTheme, Sizable,
    button::{Button, ButtonVariants},
    input::{Escape, Input, InputEvent, InputState, MoveDown, MoveUp},
};
use gpui_kit::{prelude::*, *};
use quarry_workbench::{
    CommandId, DocumentId, JumpMatch, JumpTarget, PaletteMatch, PaletteQuery, WindowId,
    jump_empty_message, palette_commands, parse_palette_input, rank_palette_commands,
    resolve_jump_targets_bounded,
};

use crate::workbench::WorkbenchController;

/// Maximum number of matches formatted and rendered in the palette list.
const MAX_VISIBLE_RESULTS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteOpenMode {
    /// Fuzzy command palette (`Cmd/Ctrl+K`); typing `#` switches seamlessly to column jump.
    Commands,
    /// Direct Go to Row or Column mode (`Cmd/Ctrl+L`).
    GoToRowOrColumn,
}

pub enum CommandPaletteEvent {
    ExecuteCommand {
        window_id: WindowId,
        doc_id: Option<DocumentId>,
        command: CommandId,
    },
    JumpToColumn {
        window_id: WindowId,
        doc_id: DocumentId,
        source_col: usize,
    },
    JumpToRow {
        window_id: WindowId,
        doc_id: DocumentId,
        row_1based: u64,
    },
    Closed,
}

#[derive(Clone)]
enum PaletteRow {
    Command {
        matched: PaletteMatch,
        enabled: bool,
        disabled_reason: Option<&'static str>,
    },
    Jump(JumpMatch),
}

pub struct CommandPalette {
    controller: Entity<WorkbenchController>,
    window_id: WindowId,
    open: bool,
    mode: PaletteOpenMode,
    captured_doc: Option<DocumentId>,
    input: Entity<InputState>,
    results_scroll: ScrollHandle,
    selected_index: usize,
    cached_is_jump: bool,
    cached_rows: Vec<PaletteRow>,
    cached_empty_msg: String,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CommandPaletteEvent> for CommandPalette {}

impl CommandPalette {
    pub fn new(
        controller: Entity<WorkbenchController>,
        window_id: WindowId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Type a command or #column (e.g. #142, amount)…")
        });
        let subscriptions = vec![cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, _, cx| match event {
                InputEvent::Change => {
                    this.selected_index = 0;
                    this.rebuild_cache(cx);
                    this.results_scroll.scroll_to_item(0);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    this.activate_selected(cx);
                }
                _ => {}
            },
        )];
        Self {
            controller,
            window_id,
            open: false,
            mode: PaletteOpenMode::Commands,
            captured_doc: None,
            input,
            results_scroll: ScrollHandle::new(),
            selected_index: 0,
            cached_is_jump: false,
            cached_rows: Vec::new(),
            cached_empty_msg: String::new(),
            _subscriptions: subscriptions,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    #[cfg(test)]
    pub fn captured_document(&self) -> Option<DocumentId> {
        self.captured_doc
    }

    pub fn open(
        &mut self,
        mode: PaletteOpenMode,
        captured_doc: Option<DocumentId>,
        initial_query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open = true;
        self.mode = mode;
        self.captured_doc = captured_doc;
        self.selected_index = 0;
        let placeholder = match mode {
            PaletteOpenMode::Commands => "Type a command or #column (e.g. #142, amount)…",
            PaletteOpenMode::GoToRowOrColumn => "Go to row (42) or column (#142 or column name)…",
        };
        self.input.update(cx, |state, cx| {
            state.set_placeholder(placeholder, window, cx);
            state.set_value(initial_query.to_owned(), window, cx);
            state.focus(window, cx);
        });
        self.rebuild_cache(cx);
        self.results_scroll.scroll_to_item(0);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            self.selected_index = 0;
            self.cached_rows.clear();
            cx.emit(CommandPaletteEvent::Closed);
            cx.notify();
        }
    }

    fn close_without_focus_event(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.selected_index = 0;
        self.cached_rows.clear();
        cx.notify();
    }

    fn rebuild_cache(&mut self, cx: &App) {
        let raw = self.input.read(cx).value().to_string();
        let jump_query = match self.mode {
            PaletteOpenMode::GoToRowOrColumn => Some((raw.as_str(), true)),
            PaletteOpenMode::Commands => match parse_palette_input(&raw) {
                PaletteQuery::Jump(q) => Some((q, false)),
                PaletteQuery::Command(_) => None,
            },
        };

        if let Some((query, allow_row_jump)) = jump_query {
            self.cached_is_jump = true;
            let jump_info = self.captured_doc.and_then(|doc_id| {
                self.controller
                    .read(cx)
                    .jump_info_for_document(self.window_id, doc_id)
            });
            let Some((headers, hidden, available_rows)) = jump_info else {
                self.cached_rows.clear();
                self.cached_empty_msg = "Open a file to jump to a row or column.".into();
                return;
            };
            let matches = resolve_jump_targets_bounded(
                query,
                &headers,
                &hidden,
                available_rows,
                allow_row_jump,
                MAX_VISIBLE_RESULTS,
            );
            self.cached_empty_msg =
                jump_empty_message(query, headers.len(), available_rows, allow_row_jump);
            self.cached_rows = matches.into_iter().map(PaletteRow::Jump).collect();
            return;
        }

        self.cached_is_jump = false;
        let query = match parse_palette_input(&raw) {
            PaletteQuery::Command(q) => q,
            PaletteQuery::Jump(q) => q,
        };
        let ranked = rank_palette_commands(query, palette_commands());
        let controller = self.controller.read(cx);
        self.cached_rows = ranked
            .into_iter()
            .take(MAX_VISIBLE_RESULTS)
            .map(|matched| {
                let (enabled, disabled_reason) = controller.evaluate_command_for_captured_document(
                    self.window_id,
                    self.captured_doc,
                    matched.command.id,
                );
                PaletteRow::Command {
                    matched,
                    enabled,
                    disabled_reason,
                }
            })
            .collect();
        self.cached_empty_msg = format!("No commands match \"{}\".", query.trim());
    }

    pub fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.cached_rows.is_empty() {
            self.selected_index = 0;
            return;
        }
        let len = self.cached_rows.len() as isize;
        let current = (self.selected_index.min(self.cached_rows.len() - 1)) as isize;
        let next = (current + delta).rem_euclid(len) as usize;
        self.selected_index = next;
        self.results_scroll.scroll_to_item(next);
        cx.notify();
    }

    fn activate_index(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.cached_rows.get(index).cloned() else {
            return;
        };
        match row {
            PaletteRow::Command {
                matched, enabled, ..
            } => {
                if !enabled {
                    return;
                }
                let cmd = matched.command.id;
                let window_id = self.window_id;
                let doc_id = self.captured_doc;
                self.close_without_focus_event(cx);
                cx.emit(CommandPaletteEvent::ExecuteCommand {
                    window_id,
                    doc_id,
                    command: cmd,
                });
            }
            PaletteRow::Jump(matched) => {
                let Some(doc_id) = self.captured_doc else {
                    return;
                };
                let window_id = self.window_id;
                let target = matched.target;
                self.close_without_focus_event(cx);
                match target {
                    JumpTarget::Column { source_col } => {
                        cx.emit(CommandPaletteEvent::JumpToColumn {
                            window_id,
                            doc_id,
                            source_col,
                        });
                    }
                    JumpTarget::Row { display_row_1based } => {
                        cx.emit(CommandPaletteEvent::JumpToRow {
                            window_id,
                            doc_id,
                            row_1based: display_row_1based,
                        });
                    }
                }
            }
        }
    }

    pub fn activate_selected(&mut self, cx: &mut Context<Self>) {
        self.activate_index(self.selected_index, cx);
    }

    #[cfg(test)]
    pub(crate) fn set_query_for_test(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_index = 0;
        self.input.update(cx, |state, cx| {
            state.set_value(query.to_owned(), window, cx)
        });
        self.rebuild_cache(cx);
        self.results_scroll.scroll_to_item(0);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn result_titles_for_test(&self) -> Vec<(String, bool)> {
        self.cached_rows
            .iter()
            .map(|row| match row {
                PaletteRow::Command {
                    matched, enabled, ..
                } => (matched.command.label.to_owned(), *enabled),
                PaletteRow::Jump(jump) => (jump.title.clone(), true),
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn selected_index_for_test(&self) -> usize {
        self.selected_index
    }

    #[cfg(test)]
    pub(crate) fn scroll_offset_y_for_test(&self) -> f32 {
        f32::from(self.results_scroll.offset().y)
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let is_macos = cfg!(target_os = "macos");
        let is_jump_mode = self.cached_is_jump;
        let selected = if self.cached_rows.is_empty() {
            0
        } else {
            self.selected_index.min(self.cached_rows.len() - 1)
        };
        let aria_title = if is_jump_mode {
            "Go to Row or Column"
        } else {
            "Command Palette"
        };
        let mode_badge = if is_jump_mode {
            "Row / Column Jump"
        } else {
            "Command Palette · prefix # to jump columns"
        };

        let mut list = div()
            .id("command-palette-results")
            .role(Role::List)
            .aria_label(format!("{aria_title} results"))
            .track_scroll(&self.results_scroll)
            .flex()
            .flex_col()
            .max_h(px(340.))
            .overflow_y_scroll()
            .py_1();

        if self.cached_rows.is_empty() {
            list = list.child(
                div()
                    .id("command-palette-empty")
                    .role(Role::Status)
                    .px_3()
                    .py_3()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(self.cached_empty_msg.clone()),
            );
        } else {
            for (idx, row) in self.cached_rows.iter().enumerate() {
                let is_selected = idx == selected;
                match row {
                    PaletteRow::Command {
                        matched,
                        enabled,
                        disabled_reason,
                    } => {
                        let shortcut = matched.command.shortcut(is_macos);
                        let bg = if is_selected {
                            cx.theme().accent
                        } else {
                            cx.theme().popover
                        };
                        let fg = if !enabled {
                            cx.theme().muted_foreground
                        } else if is_selected {
                            cx.theme().accent_foreground
                        } else {
                            cx.theme().foreground
                        };
                        let a11y_label = if let Some(reason) = disabled_reason {
                            format!(
                                "{}: {} (unavailable: {reason})",
                                matched.command.category, matched.command.label
                            )
                        } else if let Some(sc) = shortcut {
                            format!(
                                "{}: {}, {sc}",
                                matched.command.category, matched.command.label
                            )
                        } else {
                            format!("{}: {}", matched.command.category, matched.command.label)
                        };
                        list = list.child(
                            div()
                                .id(("palette-cmd-row", idx))
                                .role(Role::ListItem)
                                .aria_label(a11y_label)
                                .aria_selected(is_selected)
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .px_3()
                                .py_1p5()
                                .mx_1()
                                .rounded(px(4.))
                                .bg(bg)
                                .text_color(fg)
                                .when(*enabled, |el| el.cursor_pointer())
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        this.selected_index = idx;
                                        this.activate_index(idx, cx);
                                    }),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(cx.theme().muted_foreground)
                                                .child(matched.command.category),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(13.))
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                .child(matched.command.label),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .flex_shrink_0()
                                        .when_some(*disabled_reason, |el, reason| {
                                            el.child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(reason),
                                            )
                                        })
                                        .when_some(shortcut.filter(|_| *enabled), |el, sc| {
                                            el.child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(sc),
                                            )
                                        }),
                                ),
                        );
                    }
                    PaletteRow::Jump(jump) => {
                        let bg = if is_selected {
                            cx.theme().accent
                        } else {
                            cx.theme().popover
                        };
                        let fg = if is_selected {
                            cx.theme().accent_foreground
                        } else {
                            cx.theme().foreground
                        };
                        let a11y_label = format!("{}, {}", jump.title, jump.detail);
                        list = list.child(
                            div()
                                .id(("palette-jump-row", idx))
                                .role(Role::ListItem)
                                .aria_label(a11y_label)
                                .aria_selected(is_selected)
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .px_3()
                                .py_1p5()
                                .mx_1()
                                .rounded(px(4.))
                                .bg(bg)
                                .text_color(fg)
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| {
                                        this.selected_index = idx;
                                        this.activate_index(idx, cx);
                                    }),
                                )
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(jump.title.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(cx.theme().muted_foreground)
                                        .flex_shrink_0()
                                        .child(jump.detail.clone()),
                                ),
                        );
                    }
                }
            }
        }

        div()
            .id("command-palette-dialog")
            .role(Role::Dialog)
            .aria_label(aria_title)
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                this.close(cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.move_selection(1, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.move_selection(-1, cx);
                cx.stop_propagation();
            }))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => {
                        this.close(cx);
                        cx.stop_propagation();
                    }
                    "down" => {
                        this.move_selection(1, cx);
                        cx.stop_propagation();
                    }
                    "up" => {
                        this.move_selection(-1, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .w(px(540.))
            .max_w(px(640.))
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(px(8.))
            .shadow_lg()
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.input).small().aria_label(aria_title)),
                    )
                    .child(
                        Button::new("close-command-palette")
                            .label("×")
                            .ghost()
                            .small()
                            .accessibility_label("Close Command Palette")
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    ),
            )
            .child(list)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().secondary)
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(mode_badge)
                    .child("↑↓ navigate · Enter run · Esc close"),
            )
            .into_any_element()
    }
}
