//! Inline Find & Replace bar backed by `WorkbenchController`.

use gpui_kit::component::{
    ActiveTheme, Disableable, Sizable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{prelude::*, *};
use quarry_core::CaseSensitivity;
use quarry_workbench::{AppMessage, WindowId};

use crate::workbench::WorkbenchController;

pub enum FindBarEvent {
    Closed,
}

pub struct FindBar {
    controller: Entity<WorkbenchController>,
    window_id: WindowId,
    open: bool,
    replace_open: bool,
    match_case: bool,
    find_input: Entity<InputState>,
    replace_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FindBarEvent> for FindBar {}

impl FindBar {
    pub fn new(
        controller: Entity<WorkbenchController>,
        window_id: WindowId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let find_input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in file…"));
        let replace_input = cx.new(|cx| InputState::new(window, cx).placeholder("Replace with…"));
        let subscriptions =
            vec![
                cx.subscribe_in(&find_input, window, |this, _, event: &InputEvent, _, cx| {
                    match event {
                        InputEvent::PressEnter { secondary, shift } => {
                            if *secondary || *shift {
                                this.find_previous(cx);
                            } else {
                                this.find_next(cx);
                            }
                        }
                        InputEvent::Change => cx.notify(),
                        _ => {}
                    }
                }),
                cx.subscribe_in(
                    &replace_input,
                    window,
                    |this, _, event: &InputEvent, _, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            this.replace_current(cx);
                        }
                    },
                ),
            ];
        Self {
            controller,
            window_id,
            open: false,
            replace_open: false,
            match_case: false,
            find_input,
            replace_input,
            _subscriptions: subscriptions,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open_find(&mut self, expand_replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        if expand_replace {
            self.replace_open = true;
        }
        self.find_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            cx.emit(FindBarEvent::Closed);
            cx.notify();
        }
    }

    fn case_sensitivity(&self) -> CaseSensitivity {
        if self.match_case {
            CaseSensitivity::Sensitive
        } else {
            CaseSensitivity::Insensitive
        }
    }

    pub fn find_next(&mut self, cx: &mut Context<Self>) {
        let query = self.find_input.read(cx).value().as_bytes().to_vec();
        let case = self.case_sensitivity();
        let win = self.window_id;
        let res = self
            .controller
            .update(cx, |c, cx| c.find_next_active(win, query, case, cx));
        if let Err(error) = res {
            self.controller.update(cx, |c, cx| {
                c.set_window_message(win, Some(AppMessage::warning(error)), cx);
            });
        }
    }

    pub fn find_previous(&mut self, cx: &mut Context<Self>) {
        let query = self.find_input.read(cx).value().as_bytes().to_vec();
        let case = self.case_sensitivity();
        let win = self.window_id;
        let res = self
            .controller
            .update(cx, |c, cx| c.find_previous_active(win, query, case, cx));
        if let Err(error) = res {
            self.controller.update(cx, |c, cx| {
                c.set_window_message(win, Some(AppMessage::warning(error)), cx);
            });
        }
    }

    pub fn replace_current(&mut self, cx: &mut Context<Self>) {
        let query = self.find_input.read(cx).value().as_bytes().to_vec();
        let replacement = self.replace_input.read(cx).value().as_bytes().to_vec();
        let case = self.case_sensitivity();
        let win = self.window_id;
        let res = self.controller.update(cx, |c, cx| {
            c.replace_current_active(win, query, replacement, case, cx)
        });
        if let Err(error) = res {
            self.controller.update(cx, |c, cx| {
                c.set_window_message(win, Some(AppMessage::warning(error)), cx);
            });
        }
    }

    pub fn replace_all(&mut self, cx: &mut Context<Self>) {
        let query = self.find_input.read(cx).value().as_bytes().to_vec();
        let replacement = self.replace_input.read(cx).value().as_bytes().to_vec();
        let case = self.case_sensitivity();
        let win = self.window_id;
        let Some(doc_id) = self.controller.read(cx).scheduler().active_document(win) else {
            return;
        };
        let res = self.controller.update(cx, |c, cx| {
            c.replace_all_active(win, doc_id, query, replacement, case, cx)
        });
        if let Err(error) = res {
            self.controller.update(cx, |c, cx| {
                c.set_window_message(win, Some(error), cx);
            });
        }
    }
}

impl Render for FindBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let info = self.controller.read(cx).active_snapshot(self.window_id);
        let Some(info) = info else {
            return div().into_any_element();
        };
        let query_bytes = self.find_input.read(cx).value().as_bytes().to_vec();
        let case = self.case_sensitivity();
        let can_prev =
            self.controller
                .read(cx)
                .can_find_previous(self.window_id, &query_bytes, case);
        let can_replace =
            self.controller
                .read(cx)
                .can_replace_current(self.window_id, &query_bytes, case);
        let can_find = !query_bytes.is_empty() && !info.filter_active && !info.search_running;
        let can_replace_all = !query_bytes.is_empty() && !info.busy && !info.search_running;

        div()
            .id("find-replace-bar")
            .flex()
            .flex_col()
            .gap_1p5()
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
                            .w(px(230.))
                            .child(Input::new(&self.find_input).small().aria_label("Find text")),
                    )
                    .child(
                        Button::new("find-previous")
                            .label("Previous")
                            .small()
                            .disabled(!can_prev)
                            .on_click(cx.listener(|this, _, _, cx| this.find_previous(cx))),
                    )
                    .child(
                        Button::new("find-next")
                            .label("Next")
                            .small()
                            .primary()
                            .disabled(!can_find)
                            .on_click(cx.listener(|this, _, _, cx| this.find_next(cx))),
                    )
                    .child(
                        Checkbox::new("find-match-case")
                            .label("Match case")
                            .small()
                            .checked(self.match_case)
                            .on_change(cx.listener(|this, checked, _, cx| {
                                this.match_case = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("toggle-replace-row")
                            .label(if self.replace_open {
                                "▾ Replace"
                            } else {
                                "▸ Replace"
                            })
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.replace_open = !this.replace_open;
                                cx.notify();
                            })),
                    )
                    .when(info.search_running, |row| {
                        row.child(
                            Button::new("cancel-search")
                                .label("Cancel Search")
                                .small()
                                .danger()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let win = this.window_id;
                                    this.controller
                                        .update(cx, |c, cx| c.cancel_search_active(win, cx));
                                })),
                        )
                    })
                    .when_some(info.search_status.clone(), |row, status| {
                        row.child(
                            div()
                                .id("find-status")
                                .role(Role::Status)
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(status),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("close-find-bar")
                            .label("×")
                            .ghost()
                            .small()
                            .accessibility_label("Close Find bar")
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    ),
            )
            .when(self.replace_open, |bar| {
                bar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div().w(px(230.)).child(
                                Input::new(&self.replace_input)
                                    .small()
                                    .aria_label("Replacement text"),
                            ),
                        )
                        .child(
                            Button::new("replace-current")
                                .label("Replace")
                                .small()
                                .disabled(!can_replace)
                                .on_click(cx.listener(|this, _, _, cx| this.replace_current(cx))),
                        )
                        .child(
                            Button::new("replace-all")
                                .label("Replace All")
                                .small()
                                .disabled(!can_replace_all)
                                .on_click(cx.listener(|this, _, _, cx| this.replace_all(cx))),
                        ),
                )
            })
            .into_any_element()
    }
}
