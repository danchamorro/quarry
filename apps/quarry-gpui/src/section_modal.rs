//! Open Section modal dialog with exact checked `u64` byte inputs (From / To / Size),
//! explicit consistent binary units (`Bytes`–`TiB`), coarse 1%-step navigation, and
//! unchecked-by-default `"No quoted fields (quote = None)"` trust option (ADR 0014 / `T11`).

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme, Disableable, IndexPath, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    dialog::Cancel as DialogCancel,
    input::{Input, InputEvent, InputState},
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
};
use gpui_kit::{prelude::*, *};
use quarry_core::{
    ByteUnit, ByteWindow, OpenOptions, SectionOpenOptions, format_exact_bytes_in_unit,
    parse_exact_byte_input,
};
use quarry_workbench::format_bytes;

type OpenSectionSubmitFn =
    Box<dyn Fn(PathBuf, ByteWindow, SectionOpenOptions, &mut Window, &mut App) -> Result<(), String>>;

#[derive(Clone)]
struct UnitChoice {
    unit: ByteUnit,
}

impl SelectItem for UnitChoice {
    type Value = ByteUnit;

    fn title(&self) -> SharedString {
        self.unit.label().into()
    }

    fn value(&self) -> &ByteUnit {
        &self.unit
    }
}

type UnitSelectState = SelectState<SearchableVec<UnitChoice>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LastEditedField {
    From,
    To,
    Size,
}

struct OpenSectionDialogState {
    on_open_section: OpenSectionSubmitFn,
    path: Option<PathBuf>,
    file_size: u64,
    unit: ByteUnit,
    unit_select: Entity<UnitSelectState>,
    from_input: Entity<InputState>,
    to_input: Entity<InputState>,
    size_input: Entity<InputState>,
    from_bytes: u64,
    to_bytes: u64,
    size_bytes: u64,
    quote_free_confirmed: bool,
    suppress_sync: bool,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

impl OpenSectionDialogState {
    fn set_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.path = Some(path);
        self.file_size = size;
        let default_unit = ByteUnit::default_for_size(size);
        let default_size = size.min(10 * 1024 * 1024);
        self.from_bytes = 0;
        self.size_bytes = default_size;
        self.to_bytes = default_size;
        self.unit = default_unit;
        if let Some(idx) = ByteUnit::ALL.iter().position(|u| *u == default_unit) {
            self.unit_select.update(cx, |sel, cx| {
                sel.set_selected_index(Some(IndexPath::new(idx)), window, cx);
            });
        }
        self.sync_inputs_from_bytes(window, cx);
        self.validate_window();
        cx.notify();
    }

    fn sync_inputs_from_bytes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let factor = self.unit.factor();
        // If any endpoint isn't evenly divisible by the selected unit, switch display to Bytes so
        // exact integer values are never silently truncated.
        if factor > 1
            && (!self.from_bytes.is_multiple_of(factor)
                || !self.to_bytes.is_multiple_of(factor)
                || !self.size_bytes.is_multiple_of(factor))
        {
            self.unit = ByteUnit::Bytes;
            self.unit_select.update(cx, |sel, cx| {
                sel.set_selected_index(Some(IndexPath::new(0)), window, cx);
            });
        }
        let (from_val, _) = format_exact_bytes_in_unit(self.from_bytes, self.unit);
        let (to_val, _) = format_exact_bytes_in_unit(self.to_bytes, self.unit);
        let (size_val, _) = format_exact_bytes_in_unit(self.size_bytes, self.unit);

        self.suppress_sync = true;
        self.from_input.update(cx, |input, cx| {
            input.set_value(from_val.to_string(), window, cx);
        });
        self.to_input.update(cx, |input, cx| {
            input.set_value(to_val.to_string(), window, cx);
        });
        self.size_input.update(cx, |input, cx| {
            input.set_value(size_val.to_string(), window, cx);
        });
        self.suppress_sync = false;
    }

    fn handle_field_change(
        &mut self,
        edited: LastEditedField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.suppress_sync {
            return;
        }
        let unit = self.unit;
        let from_str = self.from_input.read(cx).value().to_string();
        let to_str = self.to_input.read(cx).value().to_string();
        let size_str = self.size_input.read(cx).value().to_string();

        match edited {
            LastEditedField::From => match parse_exact_byte_input(&from_str, unit) {
                Ok(from) => {
                    self.from_bytes = from;
                    match from.checked_add(self.size_bytes) {
                        Some(to) => {
                            self.to_bytes = to.min(self.file_size.max(from));
                            self.size_bytes = self.to_bytes.saturating_sub(self.from_bytes);
                            let (to_val, _) = format_exact_bytes_in_unit(self.to_bytes, unit);
                            let (size_val, _) = format_exact_bytes_in_unit(self.size_bytes, unit);
                            self.suppress_sync = true;
                            self.to_input.update(cx, |input, cx| {
                                input.set_value(to_val.to_string(), window, cx);
                            });
                            self.size_input.update(cx, |input, cx| {
                                input.set_value(size_val.to_string(), window, cx);
                            });
                            self.suppress_sync = false;
                            self.validate_window();
                        }
                        None => {
                            self.error = Some("From + Size overflows 64-bit byte range.".into());
                        }
                    }
                }
                Err(err) => {
                    self.error = Some(format!("From: {err}"));
                }
            },
            LastEditedField::Size => match parse_exact_byte_input(&size_str, unit) {
                Ok(size) => {
                    self.size_bytes = size;
                    match self.from_bytes.checked_add(size) {
                        Some(to) => {
                            self.to_bytes = to;
                            let (to_val, _) = format_exact_bytes_in_unit(self.to_bytes, unit);
                            self.suppress_sync = true;
                            self.to_input.update(cx, |input, cx| {
                                input.set_value(to_val.to_string(), window, cx);
                            });
                            self.suppress_sync = false;
                            self.validate_window();
                        }
                        None => {
                            self.error = Some("From + Size overflows 64-bit byte range.".into());
                        }
                    }
                }
                Err(err) => {
                    self.error = Some(format!("Size: {err}"));
                }
            },
            LastEditedField::To => match parse_exact_byte_input(&to_str, unit) {
                Ok(to) => {
                    self.to_bytes = to;
                    match to.checked_sub(self.from_bytes) {
                        Some(size) => {
                            self.size_bytes = size;
                            let (size_val, _) = format_exact_bytes_in_unit(self.size_bytes, unit);
                            self.suppress_sync = true;
                            self.size_input.update(cx, |input, cx| {
                                input.set_value(size_val.to_string(), window, cx);
                            });
                            self.suppress_sync = false;
                            self.validate_window();
                        }
                        None => {
                            self.error = Some("To offset must be >= From offset.".into());
                        }
                    }
                }
                Err(err) => {
                    self.error = Some(format!("To: {err}"));
                }
            },
        }
        cx.notify();
    }

    fn apply_coarse_start_fraction(
        &mut self,
        fraction: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_size == 0 {
            return;
        }
        let next_from = ByteWindow::slider_fraction_to_offset(fraction, self.file_size);
        let kept_len = self.size_bytes.max(1);
        self.from_bytes = next_from.min(self.file_size.saturating_sub(1));
        self.to_bytes = self
            .from_bytes
            .saturating_add(kept_len)
            .min(self.file_size);
        self.size_bytes = self.to_bytes.saturating_sub(self.from_bytes);
        self.sync_inputs_from_bytes(window, cx);
        self.validate_window();
        cx.notify();
    }

    fn nudge_start_percent(&mut self, delta_pct: i32, window: &mut Window, cx: &mut Context<Self>) {
        let current = ByteWindow::offset_to_slider_fraction(self.from_bytes, self.file_size);
        let next = (current + (delta_pct as f64 / 100.0)).clamp(0.0, 0.99);
        self.apply_coarse_start_fraction(next, window, cx);
    }

    fn validate_window(&mut self) -> Option<ByteWindow> {
        if self.path.is_none() {
            self.error = Some("Choose a file to open a section from.".into());
            return None;
        }
        if self.file_size == 0 {
            self.error = None;
            return Some(ByteWindow { base: 0, len: 0 });
        }
        let Some(window) = ByteWindow::from_range(self.from_bytes, self.to_bytes) else {
            self.error = Some("To offset must be greater than or equal to From offset.".into());
            return None;
        };
        if window.base >= self.file_size {
            self.error = Some(format!(
                "From offset ({}) must be less than file size ({}).",
                self.from_bytes, self.file_size
            ));
            return None;
        }
        if window.len == 0 {
            self.error = Some("Section size must be at least 1 byte.".into());
            return None;
        }
        if window.end().is_none() {
            self.error = Some("Byte range overflows 64-bit integer bounds.".into());
            return None;
        }
        self.error = None;
        Some(window.clamp_to_file(self.file_size))
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(byte_window) = self.validate_window() else {
            cx.notify();
            return false;
        };
        let Some(path) = self.path.clone() else {
            cx.notify();
            return false;
        };
        let options = SectionOpenOptions {
            open: OpenOptions::default(),
            encoding: None,
            parent_index: None,
            quote_free_confirmed: self.quote_free_confirmed,
            ..SectionOpenOptions::default()
        };
        let res = (self.on_open_section)(path, byte_window, options, window, cx);
        match res {
            Ok(()) => {
                window.dispatch_action(Box::new(DialogCancel), cx);
                true
            }
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                false
            }
        }
    }
}

pub fn open_section_dialog(
    initial_path: Option<PathBuf>,
    on_open_section: impl Fn(PathBuf, ByteWindow, SectionOpenOptions, &mut Window, &mut App) -> Result<(), String>
    + 'static,
    on_closed: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let initial_size = initial_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    let initial_unit = ByteUnit::default_for_size(initial_size);
    let initial_len = initial_size.min(10 * 1024 * 1024);
    let (init_from, init_to, init_size, display_unit) = {
        let f = initial_unit.factor();
        if f > 1 && !initial_len.is_multiple_of(f) {
            (0u64, initial_len, initial_len, ByteUnit::Bytes)
        } else {
            (
                0u64,
                initial_len / f.max(1),
                initial_len / f.max(1),
                initial_unit,
            )
        }
    };
    let unit_idx = ByteUnit::ALL
        .iter()
        .position(|u| *u == display_unit)
        .unwrap_or(0);
    let unit_items: Vec<UnitChoice> = ByteUnit::ALL
        .into_iter()
        .map(|unit| UnitChoice { unit })
        .collect();
    let unit_select = cx.new(|cx| {
        SelectState::new(
            SearchableVec::new(unit_items),
            Some(IndexPath::new(unit_idx)),
            window,
            cx,
        )
    });
    let from_input = cx.new(|cx| {
        let mut s = InputState::new(window, cx).placeholder("0");
        s.set_value(init_from.to_string(), window, cx);
        s
    });
    let to_input = cx.new(|cx| {
        let mut s = InputState::new(window, cx).placeholder("End offset");
        s.set_value(init_to.to_string(), window, cx);
        s
    });
    let size_input = cx.new(|cx| {
        let mut s = InputState::new(window, cx).placeholder("Section size");
        s.set_value(init_size.to_string(), window, cx);
        s
    });

    let state = cx.new(|cx| {
        let subs = vec![
            cx.subscribe_in(
                &unit_select,
                window,
                |this: &mut OpenSectionDialogState, sel, _: &SelectEvent<SearchableVec<UnitChoice>>, window, cx| {
                    if let Some(&unit) = sel.read(cx).selected_value() {
                        this.unit = unit;
                        this.sync_inputs_from_bytes(window, cx);
                        this.validate_window();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &from_input,
                window,
                |this: &mut OpenSectionDialogState, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.handle_field_change(LastEditedField::From, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &to_input,
                window,
                |this: &mut OpenSectionDialogState, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.handle_field_change(LastEditedField::To, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &size_input,
                window,
                |this: &mut OpenSectionDialogState, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.handle_field_change(LastEditedField::Size, window, cx);
                    }
                },
            ),
        ];
        OpenSectionDialogState {
            on_open_section: Box::new(on_open_section),
            path: initial_path,
            file_size: initial_size,
            unit: display_unit,
            unit_select: unit_select.clone(),
            from_input: from_input.clone(),
            to_input: to_input.clone(),
            size_input: size_input.clone(),
            from_bytes: 0,
            to_bytes: initial_len,
            size_bytes: initial_len,
            quote_free_confirmed: false,
            suppress_sync: false,
            error: None,
            _subs: subs,
        }
    });

    let on_closed = Rc::new(on_closed);
    window.open_dialog(cx, move |dialog, _, cx| {
        let on_closed_cb = on_closed.clone();
        let state_body = state.clone();
        let state_ok = state.clone();
        let can_open = {
            let st = state.read(cx);
            st.path.is_some() && st.error.is_none()
        };

        dialog
            .title("Open Section…")
            .w(px(560.))
            .overlay_closable(false)
            .close_button(true)
            .keyboard(true)
            .content(move |content, _, cx| {
                let st = state_body.read(cx);
                let path_display = st
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "No file selected".into());
                let size_summary = if st.path.is_some() {
                    format!(
                        "File size: {} ({} bytes)",
                        format_bytes(st.file_size),
                        st.file_size
                    )
                } else {
                    "Choose a delimited file to inspect a bounded byte range.".into()
                };
                let start_pct =
                    (ByteWindow::offset_to_slider_fraction(st.from_bytes, st.file_size) * 100.0)
                        .round() as u32;
                let range_summary = format!(
                    "Requested range: [{}, {}) · {} ({} bytes)",
                    st.from_bytes,
                    st.to_bytes,
                    format_bytes(st.size_bytes),
                    st.size_bytes
                );
                let unit_label = st.unit.label();
                let quote_free = st.quote_free_confirmed;
                let error_text = st.error.clone();
                let unit_select = st.unit_select.clone();
                let from_input = st.from_input.clone();
                let to_input = st.to_input.clone();
                let size_input = st.size_input.clone();
                let has_file = st.path.is_some();

                let st_browse = state_body.clone();
                let st_dec = state_body.clone();
                let st_inc = state_body.clone();
                let st_p0 = state_body.clone();
                let st_p25 = state_body.clone();
                let st_p50 = state_body.clone();
                let st_p75 = state_body.clone();
                let st_qf = state_body.clone();

                content.child(
                    div()
                        .id("open-section-dialog")
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(12.))
                                        .truncate()
                                        .child(path_display),
                                )
                                .child(
                                    Button::new("section-browse-file")
                                        .label("Browse…")
                                        .small()
                                        .on_click(move |_, window, cx| {
                                            let st_pick = st_browse.clone();
                                            let paths = cx.prompt_for_paths(PathPromptOptions {
                                                files: true,
                                                directories: false,
                                                multiple: false,
                                                prompt: Some("Choose file for Open Section".into()),
                                            });
                                            window
                                                .spawn(cx, async move |cx| {
                                                    if let Ok(Ok(Some(list))) = paths.await
                                                        && let Some(path) = list.into_iter().next()
                                                    {
                                                        let _ = cx.update(|window, cx| {
                                                            st_pick.update(cx, |s, cx| {
                                                                s.set_path(path, window, cx);
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
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(size_summary),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(64.))
                                        .text_size(px(12.))
                                        .child("Unit"),
                                )
                                .child(
                                    div().w(px(130.)).child(
                                        Select::new(&unit_select)
                                            .id("section-unit-select")
                                            .small()
                                            .accessibility_label("Byte unit"),
                                    ),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Exact 64-bit integer arithmetic (safe above 2^53 & 20 TiB)"),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_size(px(12.))
                                                .child(format!("From ({unit_label})")),
                                        )
                                        .child(
                                            Input::new(&from_input)
                                                .aria_label("Section start offset")
                                                .small()
                                                .disabled(!has_file),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_size(px(12.))
                                                .child(format!("To ({unit_label})")),
                                        )
                                        .child(
                                            Input::new(&to_input)
                                                .aria_label("Section end offset")
                                                .small()
                                                .disabled(!has_file),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_size(px(12.))
                                                .child(format!("Size ({unit_label})")),
                                        )
                                        .child(
                                            Input::new(&size_input)
                                                .aria_label("Section size")
                                                .small()
                                                .disabled(!has_file),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!("Coarse position: {start_pct}%")),
                                )
                                .child(div().flex_1())
                                .child(
                                    Button::new("section-pos-dec")
                                        .label("-1%")
                                        .accessibility_label("Decrease section start by 1 percent")
                                        .ghost()
                                        .xsmall()
                                        .disabled(!has_file)
                                        .on_click(move |_, window, cx| {
                                            st_dec.update(cx, |s, cx| {
                                                s.nudge_start_percent(-1, window, cx);
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("section-pos-inc")
                                        .label("+1%")
                                        .accessibility_label("Increase section start by 1 percent")
                                        .ghost()
                                        .xsmall()
                                        .disabled(!has_file)
                                        .on_click(move |_, window, cx| {
                                            st_inc.update(cx, |s, cx| {
                                                s.nudge_start_percent(1, window, cx);
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("section-pos-0")
                                        .label("0%")
                                        .ghost()
                                        .xsmall()
                                        .disabled(!has_file)
                                        .on_click(move |_, window, cx| {
                                            st_p0.update(cx, |s, cx| {
                                                s.apply_coarse_start_fraction(0.0, window, cx);
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("section-pos-25")
                                        .label("25%")
                                        .ghost()
                                        .xsmall()
                                        .disabled(!has_file)
                                        .on_click(move |_, window, cx| {
                                            st_p25.update(cx, |s, cx| {
                                                s.apply_coarse_start_fraction(0.25, window, cx);
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("section-pos-50")
                                        .label("50%")
                                        .ghost()
                                        .xsmall()
                                        .disabled(!has_file)
                                        .on_click(move |_, window, cx| {
                                            st_p50.update(cx, |s, cx| {
                                                s.apply_coarse_start_fraction(0.50, window, cx);
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("section-pos-75")
                                        .label("75%")
                                        .ghost()
                                        .xsmall()
                                        .disabled(!has_file)
                                        .on_click(move |_, window, cx| {
                                            st_p75.update(cx, |s, cx| {
                                                s.apply_coarse_start_fraction(0.75, window, cx);
                                            });
                                        }),
                                ),
                        )
                        .child(
                            Checkbox::new("section-quote-free")
                                .label("No quoted fields (quote = None)")
                                .checked(quote_free)
                                .on_click(move |checked, _, cx| {
                                    st_qf.update(cx, |s, cx| {
                                        s.quote_free_confirmed = *checked;
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(cx.theme().muted_foreground)
                                .child(if quote_free {
                                    "Boundaries snap outward on encoding-aligned line endings as Verified (global row unknown until Verify Boundary runs)."
                                } else {
                                    "Leaves quote parsing enabled. Uses validated checkpoints if available, or opens immediately with bounded 1 MiB Speculative alignment."
                                }),
                        )
                        .child(
                            div()
                                .id("section-range-preview")
                                .role(Role::Status)
                                .text_size(px(12.))
                                .child(range_summary),
                        )
                        .when_some(error_text, |el, err| {
                            el.child(
                                div()
                                    .id("section-dialog-error")
                                    .role(Role::Alert)
                                    .text_size(px(12.))
                                    .text_color(cx.theme().danger)
                                    .child(err),
                            )
                        }),
                )
            })
            .footer(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("section-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DialogCancel), cx);
                            }),
                    )
                    .child(
                        Button::new("section-open-confirm")
                            .label("Open Section")
                            .primary()
                            .disabled(!can_open)
                            .on_click(move |_, window, cx| {
                                state_ok.update(cx, |s, cx| {
                                    s.submit(window, cx);
                                });
                            }),
                    ),
            )
            .on_close(move |_, window, cx| {
                on_closed_cb(window, cx);
            })
    });
}
