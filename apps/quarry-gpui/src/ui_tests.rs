use std::time::{Duration, Instant};

use gpui_kit::base::test_support::snapshots;
use gpui_kit::component::Root;
use gpui_kit::component::table::TableDelegate;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    App, AppContext, ClipboardItem, Entity, Focusable, InputEvent, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollDelta, ScrollEvent, TestAppContext,
    TouchPhase, Window, WindowHandle, point, px, size,
};
use quarry_core::{HeaderMode, OpenOptions};
use quarry_workbench::{ResidencyState, WorkbenchSchedulerConfig};

use super::QuarryWindow;
use super::menus::{bind_shortcuts, install_menus};
use super::workbench::{CloseReviewAction, MAX_EDIT_BYTES, WorkbenchController};

#[cfg(target_os = "macos")]
const PRIMARY_MOD: &str = "cmd";
#[cfg(not(target_os = "macos"))]
const PRIMARY_MOD: &str = "ctrl";

fn init_test_app(cx: &mut TestAppContext) -> Entity<WorkbenchController> {
    cx.update(|cx| {
        gpui_kit::init(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        let config = WorkbenchSchedulerConfig {
            index_cache_enabled: false,
            ..Default::default()
        };
        WorkbenchController::reset_for_test(cx, config)
    })
}

fn open_test_window(
    cx: &mut TestAppContext,
    controller: &Entity<WorkbenchController>,
) -> (WindowHandle<Root>, Entity<QuarryWindow>) {
    let window_id = cx.update(|cx| controller.update(cx, |c, _| c.allocate_window()));
    let mut view = None;
    let controller_clone = controller.clone();
    let handle = cx.open_window(size(px(1100.), px(700.)), |window, cx| {
        let quarry =
            cx.new(|cx| QuarryWindow::new(window_id, controller_clone.clone(), window, cx));
        view = Some(quarry.clone());
        Root::new(quarry, window, cx)
    });
    cx.update(|cx| {
        controller.update(cx, |c, _| c.register_window_handle(window_id, handle));
    });
    (handle, view.unwrap())
}

fn wait_for_ui(
    cx: &mut TestAppContext,
    handle: WindowHandle<Root>,
    controller: &Entity<WorkbenchController>,
    view: &Entity<QuarryWindow>,
    mut ready: impl FnMut(&QuarryWindow, &mut Window, &mut App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let done = cx
            .update_window(handle.into(), |_, window, cx| {
                controller.update(cx, |c, cx| c.poll_background(cx));
                view.update(cx, |quarry, cx| quarry.sync_from_controller(window, cx));
                window.render_frame(cx);
                view.update(cx, |quarry, cx| ready(quarry, window, cx))
            })
            .unwrap();
        if done {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "GPUI window did not become ready"
        );
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[gpui_kit::test]
fn quit_shortcut_works_before_any_control_is_clicked(cx: &mut TestAppContext) {
    let controller = init_test_app(cx);
    let (handle, _) = open_test_window(cx, &controller);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.focused(cx).is_some(),
            "the window starts with an action focus path"
        );
        window.blur(cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.simulate_keystrokes(handle.into(), &format!("{PRIMARY_MOD}-q"));
    cx.run_until_parked();
    cx.update(|cx| {
        assert!(
            controller.read(cx).scheduler().total_tabs() == 0,
            "Cmd+Q must finalize cleanly before clicking any control"
        );
    });
}

#[gpui_kit::test]
fn cell_editor_is_visible_applies_through_real_ui_and_undo_restores_source(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.csv");
    let mut headers = vec!["name".to_string(), "value".to_string()];
    headers.extend((2..40).map(|column| format!("field{column}")));
    let mut original = headers.join(",") + "\n";
    for row in 0..80 {
        let mut fields = vec![
            if row == 0 {
                "first".to_string()
            } else {
                format!("row{row}")
            },
            (row + 1).to_string(),
        ];
        fields.extend((2..40).map(|column| format!("{row}:{column}")));
        original.push_str(&(fields.join(",") + "\n"));
    }
    std::fs::write(&source, &original).unwrap();
    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |quarry, cx| {
            quarry.open_paths_direct(
                vec![source.clone()],
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |quarry, _, cx| {
        quarry.snapshot(cx).is_some_and(|i| i.index_done)
            && quarry.table.read(cx).delegate().cell_at(0, 1, cx) == Some((1, 0))
    });

    cx.update_window(handle.into(), |_, window, cx| {
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
    wait_for_ui(cx, handle, &controller, &view, |quarry, window, _| {
        quarry.editing && window.try_find("apply-cell-edit").is_some()
    });
    cx.update_window(handle.into(), |_, window, cx| {
        let input = snapshots(window)
            .into_iter()
            .find(|element| element.label() == Some("Cell value"))
            .expect("the dialog must render its accessible cell editor");
        assert!(input.visible());
        assert_eq!(input.value(), Some("first"));
        window
            .within("dialog")
            .click(input.path().last().unwrap().clone(), cx);
        window.press(&format!("{PRIMARY_MOD}-a"), cx);
        cx.write_to_clipboard(ClipboardItem::new_string("x".repeat(MAX_EDIT_BYTES + 1)));
        window.press(&format!("{PRIMARY_MOD}-v"), cx);
        window.click("apply-cell-edit", cx);
        assert!(
            window.find("cell-edit-error").visible(),
            "oversized edits need a visible explanation"
        );
        window
            .within("dialog")
            .click(input.path().last().unwrap().clone(), cx);
        window.press(&format!("{PRIMARY_MOD}-a"), cx);
        cx.write_to_clipboard(ClipboardItem::new_string("edited".to_string()));
        window.press(&format!("{PRIMARY_MOD}-v"), cx);
        window.click("apply-cell-edit", cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |quarry, window, cx| {
        !quarry.editing
            && window.try_find("apply-cell-edit").is_none()
            && quarry
                .snapshot(cx)
                .is_some_and(|info| info.dirty && info.can_undo)
    });

    // Save As copy, verify disk contents and unchanged source, then Undo back to clean.
    let copy_path = directory.path().join("edited-copy.csv");
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                let win = view.read(cx).window_id;
                c.start_save_active(win, Some(copy_path.clone()), None, cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |quarry, _, cx| {
        quarry.snapshot(cx).is_some_and(|info| !info.saving)
    });
    assert!(
        std::fs::read_to_string(&copy_path)
            .unwrap()
            .contains("edited,1,")
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), original);

    cx.update_window(handle.into(), |_, window, cx| {
        window.click("undo", cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |quarry, _, cx| {
        quarry
            .snapshot(cx)
            .is_some_and(|info| !info.dirty && info.can_redo)
    });
}

#[gpui_kit::test]
fn horizontal_scroll_supports_pixels_lines_and_shift_wheel(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("wide.csv");
    let headers: Vec<String> = (0..40).map(|col| format!("col_{col}")).collect();
    let mut csv = headers.join(",") + "\n";
    for row in 0..20 {
        let fields: Vec<String> = (0..40).map(|col| format!("r{row}_c{col}")).collect();
        csv.push_str(&(fields.join(",") + "\n"));
    }
    std::fs::write(&source, csv).unwrap();
    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |quarry, cx| {
            quarry.open_paths_direct(
                vec![source.clone()],
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |quarry, _, cx| {
        quarry.snapshot(cx).is_some_and(|i| i.index_done)
            && quarry.table.read(cx).delegate().cell_at(0, 1, cx) == Some((1, 0))
    });

    cx.update_window(handle.into(), |_, window, cx| {
        assert!(window.try_find(("col-header", 16usize)).is_none());
        window.scroll("table", ScrollDelta::Pixels(point(px(-1400.), px(0.))), cx);
        let after_pixels = f32::from(
            view.read(cx)
                .table
                .read(cx)
                .horizontal_scroll_handle
                .offset()
                .x,
        );
        assert!(after_pixels < -500.0);

        window.scroll("table", ScrollDelta::Lines(point(-6.0, 0.0)), cx);
        let after_lines = f32::from(
            view.read(cx)
                .table
                .read(cx)
                .horizontal_scroll_handle
                .offset()
                .x,
        );
        assert!(after_lines < after_pixels);

        let table_center = window.find("table").bounds().center();
        window.dispatch_event(
            ScrollEvent {
                position: table_center,
                delta: ScrollDelta::Lines(point(0.0, -8.0)),
                modifiers: Modifiers {
                    shift: true,
                    ..Default::default()
                },
                touch_phase: TouchPhase::Moved,
                ..ScrollEvent::default()
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        let after_shift = f32::from(
            view.read(cx)
                .table
                .read(cx)
                .horizontal_scroll_handle
                .offset()
                .x,
        );
        assert!(after_shift < after_lines);
    })
    .unwrap();
}

#[gpui_kit::test]
fn nested_select_switches_directly_to_another_toolbar_panel_in_one_click_with_focus(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("panels.csv");
    std::fs::write(&source, "city,count\nReno,10\nAustin,20\n").unwrap();
    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |quarry, cx| {
            quarry.open_paths_direct(
                vec![source.clone()],
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |quarry, _, cx| {
        quarry.snapshot(cx).is_some_and(|i| i.index_done)
            && quarry.table.read(cx).delegate().cell_at(0, 1, cx) == Some((1, 0))
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("format-popover", 0usize), cx);
        assert!(window.find("format-delimiter").visible());
        window.click("format-delimiter", cx);
        window.render_frame(cx);

        window.click(("columns-popover", 0usize), cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find("auto-fit-columns")
                .is_some_and(|el| el.visible()),
            "switching from an open nested Select to Columns must take only one click"
        );
        assert!(window.try_find("format-delimiter").is_none());
        assert!(window.focused(cx).is_some());

        window.press("escape", cx);
        window.render_frame(cx);
        assert!(window.try_find("auto-fit-columns").is_none());
        assert!(window.focused(cx).is_some());
    })
    .unwrap();
}

#[gpui_kit::test]
fn accessibility_exposes_grid_cell_labels_and_restores_focus_after_editor(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("a11y.csv");
    std::fs::write(
        &source,
        "city,population,notes\nReno,264165,\nAustin,961855,TX\n",
    )
    .unwrap();
    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |quarry, cx| {
            quarry.open_paths_direct(
                vec![source.clone()],
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |quarry, _, cx| {
        quarry.snapshot(cx).is_some_and(|i| i.index_done)
            && quarry.table.read(cx).delegate().cell_at(0, 1, cx) == Some((1, 0))
    });

    cx.update_window(handle.into(), |_, window, cx| {
        let table = view.read(cx).table.read(cx);
        assert_eq!(
            table.delegate().cell_label(0, 0, cx).as_deref(),
            Some("Row 1")
        );
        assert_eq!(
            table.delegate().cell_label(0, 1, cx).as_deref(),
            Some("city: Reno")
        );
        assert_eq!(
            table.delegate().cell_label(0, 3, cx).as_deref(),
            Some("notes: empty")
        );
        assert_eq!(table.delegate().cell_label(500, 1, cx), None);
        assert!(
            snapshots(window)
                .iter()
                .any(|el| el.label() == Some("a11y.csv data grid")),
            "data grid must expose an accessible label naming the active file"
        );
    })
    .unwrap();

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |quarry, cx| quarry.edit_cell(0, 1, window, cx));
        window.render_frame(cx);
        assert!(window.find("cancel-cell-edit").visible());
        window.click("cancel-cell-edit", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let table_focus = view.read(cx).table.focus_handle(cx);
        assert!(
            table_focus.contains_focused(window, cx),
            "closing the cell editor must restore keyboard focus to the data grid"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn multi_window_move_tab_alias_routing_and_cross_window_quit_review(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let file_a = directory.path().join("alpha.csv");
    let file_b = directory.path().join("beta.csv");
    let alias_a = directory.path().join("alpha-hardlink.csv");
    std::fs::write(&file_a, "name,val\nA,1\n").unwrap();
    std::fs::write(&file_b, "name,val\nB,2\n").unwrap();
    std::fs::hard_link(&file_a, &alias_a).unwrap();

    let controller = init_test_app(cx);
    let (h1, v1) = open_test_window(cx, &controller);
    let (h2, v2) = open_test_window(cx, &controller);
    let w1 = cx.update(|cx| v1.read(cx).window_id);
    let w2 = cx.update(|cx| v2.read(cx).window_id);

    // Open both files in Window 1 lazily, then move `beta.csv` to Window 2.
    cx.update_window(h1.into(), |_, window, cx| {
        v1.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![file_a.clone(), file_b.clone()],
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, h1, &controller, &v1, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.index_done && i.filename == "alpha.csv")
    });

    let tab_b = cx.update(|cx| controller.read(cx).scheduler().tabs_in_window(w1)[1]);
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.move_tab_to_window(tab_b, w2, cx))
            .unwrap();
    });
    wait_for_ui(cx, h2, &controller, &v2, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.index_done && i.filename == "beta.csv")
    });

    // Opening hard-link alias of alpha.csv in Window 2 must route back to Window 1's owner.
    let (owner_win, _) = cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_paths(w2, vec![alias_a.clone()], OpenOptions::default(), None, cx)
            })
            .unwrap()
    });
    assert_eq!(
        owner_win, w1,
        "hard-link alias must resolve to owning Window 1"
    );
    assert_eq!(
        cx.update(|cx| controller.read(cx).scheduler().total_tabs()),
        2
    );

    // Dirty both documents across the two windows, request Quit, then Keep Editing (Cancel):
    // both dirty documents must stay open and intact.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.apply_cell_edit(w1, 1, 0, "A-mod".into(), cx))
            .unwrap();
        controller
            .update(cx, |c, cx| c.apply_cell_edit(w2, 1, 0, "B-mod".into(), cx))
            .unwrap();
    });
    let review = cx.update(|cx| controller.update(cx, |c, cx| c.request_quit(cx)));
    let token = match review {
        CloseReviewAction::ConfirmTab {
            review_token,
            window_id,
            ..
        } => {
            assert_eq!(window_id, w1);
            review_token
        }
        other => panic!("expected ConfirmTab for w1, got {other:?}"),
    };
    // Overlapping close request from Window 2 must wait instead of overwriting Window 1's review.
    let overlap = cx.update(|cx| controller.update(cx, |c, cx| c.request_close_window(w2, cx)));
    assert!(matches!(overlap, CloseReviewAction::Wait { .. }));
    cx.update(|cx| controller.update(cx, |c, cx| c.cancel_close_review(Some(token), cx)));
    cx.update(|cx| {
        let sched = controller.read(cx).scheduler();
        assert_eq!(sched.dirty_tabs().len(), 2);
        assert_eq!(sched.total_tabs(), 2);
    });

    // Quit must wait while another window has an active unapplied editor dialog open.
    cx.update(|cx| controller.update(cx, |c, cx| c.set_window_editing(w2, true, cx)));
    let blocked_quit = cx.update(|cx| controller.update(cx, |c, cx| c.request_quit(cx)));
    assert!(matches!(blocked_quit, CloseReviewAction::Wait { .. }));
    cx.update(|cx| controller.update(cx, |c, cx| c.set_window_editing(w2, false, cx)));

    // Close Window 2 after approving discard; Window 1's dirty document must survive cleanly.
    let w2_review = cx.update(|cx| controller.update(cx, |c, cx| c.request_close_window(w2, cx)));
    let w2_token = match w2_review {
        CloseReviewAction::ConfirmTab {
            review_token,
            window_id,
            ..
        } => {
            assert_eq!(window_id, w2);
            review_token
        }
        other => panic!("expected ConfirmTab for w2, got {other:?}"),
    };
    let done =
        cx.update(|cx| controller.update(cx, |c, cx| c.approve_discard_in_review(w2_token, cx)));
    assert_eq!(done, CloseReviewAction::Proceed);
    cx.update(|cx| {
        let sched = controller.read(cx).scheduler();
        assert_eq!(sched.total_tabs(), 1);
        assert_eq!(sched.dirty_tabs().len(), 1);
        assert!(sched.active_document(w1).is_some());
    });
}

#[gpui_kit::test]
fn bulk_lazy_open_and_header_rename_copy_whitespace_work(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for idx in 0..15 {
        let p = directory.path().join(format!("bulk_{idx:02}.csv"));
        std::fs::write(&p, format!("city,note\nReno,hello  world_{idx}\n")).unwrap();
        paths.push(p);
    }

    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                paths.clone(),
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.index_done)
    });

    cx.update(|cx| {
        let sched = controller.read(cx).scheduler();
        assert_eq!(sched.total_tabs(), 15);
        assert_eq!(sched.hydrated_count(), 1);
        assert_eq!(sched.stub_count(), 14);
        assert_eq!(
            sched.residency(sched.tabs_in_window(win_id)[14]),
            Some(ResidencyState::Stub)
        );
    });

    // Rename header, select cell, copy, toggle Show All Whitespace, and confirm clean state.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.rename_header_active(win_id, 0, "town".into(), cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.headers.first().map(String::as_str) == Some("town") && i.dirty)
    });

    cx.update_window(handle.into(), |_, window, cx| {
        let target = view.read(cx).table.read(cx).delegate().cell_at(0, 1, cx);
        controller.update(cx, |c, _| {
            c.set_active_selection(
                win_id,
                target.map(|(r, c)| quarry_workbench::GridSelection::Cell { row: r, column: c }),
            );
        });
        let copied = controller.read(cx).copy_active_selection(win_id).unwrap();
        assert_eq!(copied, "Reno");
        window.click("toggle-whitespace", cx);
        assert!(controller.read(cx).show_all_whitespace());
    })
    .unwrap();
}

#[gpui_kit::test]
fn in_place_save_reloads_and_multi_tab_close_review_advances_after_save(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let file_a = directory.path().join("first.csv");
    let file_b = directory.path().join("second.csv");
    std::fs::write(&file_a, "name,val\nA,1\n").unwrap();
    std::fs::write(&file_b, "name,val\nB,2\n").unwrap();

    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    // Track controller notifications to verify the finishing poll tick notifies observers.
    let notified = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let notified_flag = notified.clone();
    let _observer = cx.update(|cx| {
        cx.observe(&controller, move |_, _| {
            notified_flag.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    });

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![file_a.clone(), file_b.clone()],
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    let before_poll = notified.load(std::sync::atomic::Ordering::SeqCst);
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.index_done && i.filename == "first.csv")
    });
    assert!(
        notified.load(std::sync::atomic::Ordering::SeqCst) > before_poll,
        "controller must notify observers when indexing finishes cleanly"
    );

    // Dirty both tabs in the same window.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.apply_cell_edit(win_id, 1, 0, "A-saved".into(), cx)
            })
            .unwrap();
    });
    let tab_b = cx.update(|cx| controller.read(cx).scheduler().tabs_in_window(win_id)[1]);
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.activate_tab(win_id, tab_b, cx))
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.index_done && i.filename == "second.csv")
    });
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.apply_cell_edit(win_id, 1, 0, "B-dirty".into(), cx)
            })
            .unwrap();
    });

    // Request closing the window: first dirty tab prompts Save / Discard / Keep Editing.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| q.close_window_requested(window, cx));
        window.render_frame(cx);
        assert!(window.find("close-save").visible());
        window.click("close-save", cx);
    })
    .unwrap();

    // Once the first tab finishes saving, the second dirty tab must become active AND its
    // close-review confirmation dialog must be presented automatically.
    wait_for_ui(cx, handle, &controller, &view, |q, window, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.filename == "second.csv" && i.dirty)
            && q.confirming
            && window.find("close-discard").visible()
    });
    assert!(
        std::fs::read_to_string(&file_a)
            .unwrap()
            .contains("A-saved")
    );

    // Keep Editing cancels cleanly; first tab is already clean & reloaded on disk.
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("close-keep-editing", cx);
    })
    .unwrap();
    let tab_a = cx.update(|cx| controller.read(cx).scheduler().tabs_in_window(win_id)[0]);
    assert!(!cx.update(|cx| controller.read(cx).scheduler().tab_is_dirty(tab_a)));
}
