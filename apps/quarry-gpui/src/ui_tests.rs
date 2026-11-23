use std::time::{Duration, Instant};

use gpui_kit::base::test_support::snapshots;
use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    App, AppContext, ClipboardItem, Entity, InputEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ScrollDelta, TestAppContext, Window, WindowHandle, point, px,
    size,
};
use quarry_core::{HeaderMode, OpenOptions};

use super::{Quarry, bind_shortcuts, grid::Command};

#[gpui_kit::test]
fn quit_shortcut_works_before_any_control_is_clicked(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(bind_shortcuts);
    let handle = cx.open_window(size(px(1100.), px(700.)), |window, cx| {
        let quarry = cx.new(|cx| Quarry::new(window, cx));
        Root::new(quarry, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.focused(cx).is_some(),
            "the window starts with an action focus path"
        );
        // Closing a toolbar panel blurs its nested input before the next frame.
        window.blur(cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(handle.into(), "cmd-q");
    cx.run_until_parked();
    cx.update(|cx| {
        assert!(
            cx.windows().is_empty(),
            "Cmd+Q must work before clicking a control"
        )
    });
}

fn wait_for_ui(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    view: &Entity<Quarry>,
    mut ready: impl FnMut(&Quarry, &mut Window, &mut App) -> bool,
) {
    // The engine uses real worker threads, so give them wall-clock time while rendering
    // actual application frames and running the application's normal response handler.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let done = cx
            .update_window(handle.into(), |_, window, cx| {
                view.update(cx, |quarry, cx| {
                    quarry.poll(window, cx);
                });
                window.render_frame(cx);
                view.update(cx, |quarry, cx| {
                    let done = ready(quarry, window, cx);
                    if !done && let Some((message, true)) = &quarry.message {
                        panic!("comparison error before expected UI state: {message}");
                    }
                    done
                })
            })
            .unwrap();
        if done {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "comparison UI did not become ready"
        );
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[gpui_kit::test]
fn cell_editor_is_visible_applies_through_real_ui_and_undo_restores_source(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.csv");
    let mut headers = vec!["name".to_string(), "value".to_string()];
    headers.extend((2..300).map(|column| format!("field{column}")));
    let mut original = headers.join(",") + "\n";
    for row in 0..100 {
        let mut fields = vec![
            if row == 0 {
                "first".to_string()
            } else {
                format!("row{row}")
            },
            (row + 1).to_string(),
        ];
        fields.extend((2..300).map(|column| format!("{row}:{column}")));
        original.push_str(&(fields.join(",") + "\n"));
    }
    std::fs::write(&source, &original).unwrap();
    cx.update(gpui_kit::init);
    let mut view = None;
    let handle = cx.open_window(size(px(1100.), px(700.)), |window, cx| {
        let quarry = cx.new(|cx| Quarry::new(window, cx));
        quarry.update(cx, |quarry, cx| {
            quarry.command(
                Command::Open(
                    source.clone(),
                    OpenOptions {
                        header_mode: HeaderMode::FirstRow,
                        ..OpenOptions::default()
                    },
                ),
                cx,
            );
        });
        view = Some(quarry.clone());
        Root::new(quarry, window, cx)
    });
    let view = view.unwrap();
    wait_for_ui(cx, handle, &view, |quarry, _, cx| {
        !quarry.busy && quarry.table.read(cx).delegate().cell_at(0, 1) == Some((1, 0))
    });
    cx.update_window(handle.into(), |_, window, cx| {
        assert!(window.try_find(("col-header", 20usize)).is_none());
        window.scroll("table", ScrollDelta::Pixels(point(px(-2200.), px(0.))), cx);
        assert!(
            (17usize..32).any(|column| window.try_find(("col-header", column)).is_some()),
            "horizontal wheel input must reveal columns beyond the initial viewport"
        );
        window.scroll("table", ScrollDelta::Pixels(point(px(2200.), px(0.))), cx);
        assert!(window.find(("col-header", 1usize)).visible());
    })
    .unwrap();
    wait_for_ui(cx, handle, &view, |quarry, _, cx| {
        quarry.table.read(cx).delegate().cell_at(0, 1) == Some((1, 0))
    });
    cx.update_window(handle.into(), |_, window, cx| {
        // Hit the first cell using its rendered row and column geometry. Dispatch both
        // native clicks so the TableEvent subscription, worker read, and dialog all run.
        let column = window.find(("col-header", 1usize)).bounds();
        let row = window.find(("row", 0usize)).bounds();
        let position = point(column.center().x, row.center().y);
        window.dispatch_event(
            MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers: Default::default(),
            }
            .to_platform_input(),
            cx,
        );
        for click_count in 1..=2 {
            window.dispatch_event(
                MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Default::default(),
                    click_count,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Default::default(),
                    click_count,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
        }
    })
    .unwrap();
    wait_for_ui(cx, handle, &view, |quarry, window, _| {
        !quarry.busy && quarry.editing && window.try_find("apply-cell-edit").is_some()
    });
    cx.update_window(handle.into(), |_, window, cx| {
        let apply = window.find("apply-cell-edit");
        let cancel = window.find("cancel-cell-edit");
        assert!(
            apply.visible() && cancel.visible(),
            "the editor needs a reachable footer"
        );
        assert_eq!(apply.label(), Some("Apply edit"));
        let input = snapshots(window)
            .into_iter()
            .find(|element| element.label() == Some("Cell value"))
            .expect("the dialog must render its accessible cell editor");
        assert!(input.visible());
        assert_eq!(input.value(), Some("first"));
        window
            .within("dialog")
            .click(input.path().last().unwrap().clone(), cx);
        window.press("cmd-a", cx);
        cx.write_to_clipboard(ClipboardItem::new_string(
            "x".repeat(super::document::MAX_EDIT_BYTES + 1),
        ));
        window.press("cmd-v", cx);
        window.click("apply-cell-edit", cx);
        assert!(
            window.find("cell-edit-error").visible(),
            "oversized edits need a visible explanation"
        );
        assert!(
            window.find("apply-cell-edit").visible(),
            "invalid edits keep the editor open"
        );
        window
            .within("dialog")
            .click(input.path().last().unwrap().clone(), cx);
        window.press("cmd-a", cx);
        window.input("Edited café", cx);
        window.click("apply-cell-edit", cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &view, |quarry, window, _| {
        !quarry.busy
            && !quarry.editing
            && quarry
                .info
                .as_ref()
                .is_some_and(|info| info.dirty && info.can_undo)
            && window.try_find("apply-cell-edit").is_none()
    });
    let edited = directory.path().join("edited.csv");
    view.update(cx, |quarry, cx| {
        quarry.command(Command::SaveCopy(edited.clone()), cx)
    });
    wait_for_ui(cx, handle, &view, |quarry, _, _| !quarry.busy);
    assert!(
        std::fs::read_to_string(&edited)
            .unwrap()
            .contains("Edited café,1")
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
    cx.update_window(handle.into(), |_, window, cx| window.click("undo", cx))
        .unwrap();
    wait_for_ui(cx, handle, &view, |quarry, _, _| {
        !quarry.busy
            && quarry
                .info
                .as_ref()
                .is_some_and(|info| !info.dirty && info.can_redo)
    });
    let restored = directory.path().join("restored.csv");
    view.update(cx, |quarry, cx| {
        quarry.command(Command::SaveCopy(restored.clone()), cx)
    });
    wait_for_ui(cx, handle, &view, |quarry, _, _| !quarry.busy);
    assert_eq!(std::fs::read_to_string(restored).unwrap(), original);

    view.update(cx, |quarry, cx| quarry.command(Command::Cell(1, 0), cx));
    wait_for_ui(cx, handle, &view, |quarry, window, _| {
        !quarry.busy && quarry.editing && window.try_find("apply-cell-edit").is_some()
    });
    // The original changes after the editor is opened, a real asynchronous edit failure.
    std::fs::write(&source, "changed externally\n").unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        let input = snapshots(window)
            .into_iter()
            .find(|element| element.label() == Some("Cell value"))
            .unwrap();
        window
            .within("dialog")
            .click(input.path().last().unwrap().clone(), cx);
        window.press("cmd-a", cx);
        window.input("Keep this draft", cx);
        window.click("apply-cell-edit", cx);
        assert!(
            window.find("apply-cell-edit").visible(),
            "wait for engine success before closing"
        );
    })
    .unwrap();
    wait_for_ui(cx, handle, &view, |quarry, window, _| {
        !quarry.busy && quarry.editing && window.try_find("cell-edit-error").is_some()
    });
    cx.update_window(handle.into(), |_, window, _| {
        let input = snapshots(window)
            .into_iter()
            .find(|element| element.label() == Some("Cell value"))
            .unwrap();
        assert_eq!(input.value(), Some("Keep this draft"));
        assert!(window.find("cell-edit-error").visible());
        assert!(window.find("cancel-cell-edit").visible());
    })
    .unwrap();
}
