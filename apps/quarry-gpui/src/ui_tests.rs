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
                let doc_id = c.scheduler().active_document(win);
                c.start_save_active(win, doc_id, Some(copy_path.clone()), None, cx)
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
            .update(cx, |c, cx| {
                let doc1 = c.scheduler().active_document(w1).unwrap();
                let doc2 = c.scheduler().active_document(w2).unwrap();
                // Mismatched doc IDs must be rejected without modifying the active document.
                assert!(
                    c.apply_cell_edit(w1, doc2, 1, 0, "wrong".into(), cx)
                        .is_err()
                );
                assert!(
                    c.rename_header_active(w1, doc2, 0, "wrong".into(), cx)
                        .is_err()
                );
                assert!(c.start_save_active(w1, Some(doc2), None, None, cx).is_err());
                c.apply_cell_edit(w1, doc1, 1, 0, "A-mod".into(), cx)
            })
            .unwrap();
        controller
            .update(cx, |c, cx| {
                let doc2 = c.scheduler().active_document(w2).unwrap();
                c.apply_cell_edit(w2, doc2, 1, 0, "B-mod".into(), cx)
            })
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
                let doc_id = c.scheduler().active_document(win_id).unwrap();
                c.rename_header_active(win_id, doc_id, 0, "town".into(), cx)
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
    let tab_a = cx.update(|cx| controller.read(cx).scheduler().tabs_in_window(win_id)[0]);
    let tab_b = cx.update(|cx| controller.read(cx).scheduler().tabs_in_window(win_id)[1]);
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.apply_cell_edit(win_id, tab_a, 1, 0, "A-saved".into(), cx)
            })
            .unwrap();
    });
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
                c.apply_cell_edit(win_id, tab_b, 1, 0, "B-dirty".into(), cx)
            })
            .unwrap();
    });

    // Verify cancelling a window review after approving Discard on tab 1 revokes tab 1's approval.
    let first_review =
        cx.update(|cx| controller.update(cx, |c, cx| c.request_close_window(win_id, cx)));
    let first_token = match first_review {
        CloseReviewAction::ConfirmTab { review_token, .. } => review_token,
        other => panic!("expected ConfirmTab, got {other:?}"),
    };
    let second_review =
        cx.update(|cx| controller.update(cx, |c, cx| c.approve_discard_in_review(first_token, cx)));
    let second_token = match second_review {
        CloseReviewAction::ConfirmTab { review_token, .. } => review_token,
        other => panic!("expected ConfirmTab for second tab, got {other:?}"),
    };
    assert!(cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .tab(tab_a)
            .unwrap()
            .close_ready
    }));
    cx.update(|cx| controller.update(cx, |c, cx| c.cancel_close_review(Some(second_token), cx)));
    assert!(
        !cx.update(|cx| controller
            .read(cx)
            .scheduler()
            .tab(tab_a)
            .unwrap()
            .close_ready),
        "cancelling window review must revoke earlier discard approvals"
    );

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

#[gpui_kit::test]
fn find_replace_and_numeric_filter_export_work_through_gpui_ui(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("find_filter.csv");
    std::fs::write(
        &source,
        "item,price,tag\napple,10,fruit\nbanana,25,fruit\ncarrot,5,veg\napple pie,40,dessert\n",
    )
    .unwrap();

    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
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
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.index_done)
    });

    // Open Find bar via toolbar button and verify Find & Replace controls appear.
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("toolbar-find", cx);
        window.render_frame(cx);
        assert!(window.find("find-next").visible());
        assert!(window.find("close-find-bar").visible());
        window.click("toggle-replace-row", cx);
        window.render_frame(cx);
        assert!(window.find("replace-all").visible());
    })
    .unwrap();

    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.find_next_active(
                    win_id,
                    b"apple".to_vec(),
                    quarry_core::CaseSensitivity::Insensitive,
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| !i.search_running && i.search_status.is_some())
            && q.table.read(cx).selected_cell() == Some((0, 1))
    });

    // Replace Current on first "apple" -> "pear", then Replace All "fruit" -> "produce".
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.replace_current_active(
                    win_id,
                    b"apple".to_vec(),
                    b"pear".to_vec(),
                    quarry_core::CaseSensitivity::Insensitive,
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| !i.search_running && i.dirty)
    });
    let doc_id = cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .active_document(win_id)
            .unwrap()
    });
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.replace_all_active(
                    win_id,
                    doc_id,
                    b"fruit".to_vec(),
                    b"produce".to_vec(),
                    quarry_core::CaseSensitivity::Sensitive,
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| !i.structural_running && i.index_done)
            && controller
                .read(cx)
                .active_document(win_id)
                .and_then(|d| d.effective_cell(1, 2))
                == Some(b"produce".as_slice())
    });

    // Close Find bar, apply numeric Between filter [10..30], export filtered rows, then clear.
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("close-find-bar", cx);
    })
    .unwrap();
    let query = quarry_core::FilterQuery {
        predicates: vec![quarry_core::FilterPredicate {
            column: 1,
            operator: quarry_core::FilterOperator::Between,
            value: b"10".to_vec(),
            upper_bound: Some(b"30".to_vec()),
        }],
        case_sensitivity: quarry_core::CaseSensitivity::Insensitive,
    };
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.start_filter_active(win_id, query, cx))
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.filter_active && i.filter_done && i.can_export_filtered)
    });
    assert_eq!(
        cx.update(|cx| view.read(cx).snapshot(cx).unwrap().available_rows),
        2
    );

    let export_path = directory.path().join("between-export.csv");
    let current_doc = cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .active_document(win_id)
            .unwrap()
    });
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_filtered_export_active(win_id, current_doc, export_path.clone(), cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| !i.export_running)
    });
    let exported = std::fs::read_to_string(&export_path).unwrap();
    assert!(exported.contains("pear,10,produce"));
    assert!(exported.contains("banana,25,produce"));
    assert!(!exported.contains("carrot"));
    assert!(!exported.contains("apple pie"));
}

#[gpui_kit::test]
fn sort_duplicates_cleanup_and_structural_ops_work_through_gpui_ui(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("structured.csv");
    std::fs::write(
        &source,
        "code,score,label\n  z-1  ,30,hi\na-2,10,dup\na-2,10,dup\nb-3,20,mid\n",
    )
    .unwrap();

    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
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
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.index_done)
    });

    // 1. Clean Selected Columns (Trim col 0) via dialog UI.
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("toolbar-cleanup", cx);
        window.render_frame(cx);
        assert!(window.find("apply-cleanup").visible());
        window.click("apply-cleanup", cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| !i.structural_running && i.index_done)
            && controller
                .read(cx)
                .active_document(win_id)
                .and_then(|d| d.effective_cell(1, 0))
                == Some(b"z-1".as_slice())
    });

    // 2. Find & Remove Duplicates via dialog UI + Duplicate Preview confirmation banner.
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("toolbar-duplicates", cx);
        window.render_frame(cx);
        assert!(window.find("apply-duplicates").visible());
        window.click("apply-duplicates", cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, window, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.duplicate_preview.is_some())
            && window.find("confirm-remove-duplicates").visible()
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("confirm-remove-duplicates", cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.duplicate_preview.is_none() && i.total_rows == 3 && i.index_done)
    });

    // 3. Sort Rows via dialog UI (defaults to col 0 Text Ascending -> a-2, b-3, z-1).
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("toolbar-sort", cx);
        window.render_frame(cx);
        assert!(window.find("apply-sort").visible());
        window.click("apply-sort", cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| !i.structural_running && i.index_done)
            && controller
                .read(cx)
                .active_document(win_id)
                .and_then(|d| d.effective_cell(1, 0))
                == Some(b"a-2".as_slice())
    });

    // 4. Split col 0 on "-" -> produces 4 columns ("a", "2", ...), then Combine cols 0+1 on "_".
    let doc_id = cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .active_document(win_id)
            .unwrap()
    });
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_split_column_active(win_id, doc_id, 0, b"-".to_vec(), cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| !i.structural_running && i.index_done && i.headers.len() == 4)
    });
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_combine_columns_active(win_id, doc_id, vec![0, 1], b"_".to_vec(), cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| !i.structural_running && i.index_done && i.headers.len() == 3)
            && controller
                .read(cx)
                .active_document(win_id)
                .and_then(|d| d.effective_cell(1, 0))
                == Some(b"a_2".as_slice())
    });

    // 5. Insert 1 row at position 1, then select row 1 and delete it.
    let mut insert_dlg = quarry_workbench::StructuralDialog::insert_rows(0);
    insert_dlg.count = "1".into();
    insert_dlg.position = "1".into();
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_insert_rows_active(win_id, doc_id, &insert_dlg, cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| !i.structural_running && i.index_done && i.total_rows == 4)
    });
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.select_row_active(win_id, 1, false, false, cx);
            c.start_delete_selected_rows_active(win_id, cx).unwrap();
        });
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| !i.structural_running && i.index_done && i.total_rows == 3)
    });

    // 6. Move col 2 to position 0, then delete col 0.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_move_columns_active(win_id, doc_id, vec![2], 0, cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| {
            !i.structural_running
                && i.index_done
                && i.headers.first().map(String::as_str) == Some("label")
        })
    });
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_delete_selected_columns_active(win_id, vec![0], cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| !i.structural_running && i.index_done && i.headers.len() == 2)
    });
}

#[gpui_kit::test]
fn find_escape_header_column_default_and_cross_tab_reveal_isolation(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let path_a = directory.path().join("tab-a.csv");
    let path_b = directory.path().join("tab-b.csv");
    std::fs::write(&path_a, "col_a,col_b\nfoo,1\nbar,2\n").unwrap();
    std::fs::write(&path_b, "x,y\n10,20\n30,40\n").unwrap();

    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path_a.clone(), path_b.clone()],
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
    let tabs = cx.update(|cx| controller.read(cx).tabs_for_window(win_id));
    assert_eq!(tabs.len(), 2);
    let doc_a = tabs[0].id;
    let doc_b = tabs[1].id;

    // 1. Escape closes the Find & Replace bar from both Find and Replace inputs and restores grid focus.
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("toolbar-find", cx);
        window.render_frame(cx);
        assert!(view.read(cx).find_bar.read(cx).is_open());
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(!view.read(cx).find_bar.read(cx).is_open());
        assert!(
            view.read(cx)
                .table
                .focus_handle(cx)
                .contains_focused(window, cx)
        );

        view.update(cx, |q, cx| {
            q.find_bar.update(cx, |bar, cx| {
                bar.open_find(true, window, cx);
                bar.focus_replace_for_test(window, cx);
            });
        });
        window.render_frame(cx);
        assert!(view.read(cx).find_bar.read(cx).is_open());
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(!view.read(cx).find_bar.read(cx).is_open());
        assert!(
            view.read(cx)
                .table
                .focus_handle(cx)
                .contains_focused(window, cx)
        );
    })
    .unwrap();

    // 2. Selecting a column header overrides an earlier selected cell for dialog defaults.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.table.update(cx, |t, cx| t.set_selected_cell(0, 1, cx));
        });
        controller.update(cx, |c, cx| {
            c.select_column_active(win_id, 1, false, false, cx);
        });
        window.render_frame(cx);
        assert_eq!(view.read(cx).active_selected_column(cx), 1);
    })
    .unwrap();

    // 3. A pending search reveal for Tab A must NOT move Tab B's selection, and applies once Tab A wakes.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.activate_tab(win_id, doc_b, cx))
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.document_id == doc_b && i.index_done)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.table.update(cx, |t, cx| t.set_selected_cell(0, 1, cx));
        });
        controller.update(cx, |c, cx| {
            c.queue_pending_reveal_cell_for_test(doc_a, (2, 1));
            cx.notify();
        });
        window.render_frame(cx);
        assert_eq!(view.read(cx).table.read(cx).selected_cell(), Some((0, 1)));
    })
    .unwrap();
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.activate_tab(win_id, doc_a, cx))
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.document_id == doc_a)
            && q.table.read(cx).selected_cell() == Some((1, 2))
    });
}

/// Minimal RFC 1952 gzip encoder using uncompressed DEFLATE stored blocks (BTYPE=00)
/// so UI tests can exercise real `.gz` streaming without adding `flate2` to `quarry-gpui`.
fn encode_stored_gzip(data: &[u8]) -> Vec<u8> {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if (crc & 1) != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc ^= 0xffff_ffff;

    let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
    let mut offset = 0;
    loop {
        let remaining = data.len().saturating_sub(offset);
        let chunk_len = remaining.min(65_535);
        let is_final = offset + chunk_len >= data.len();
        out.push(u8::from(is_final));
        let len = chunk_len as u16;
        let nlen = !len;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(&data[offset..offset + chunk_len]);
        offset += chunk_len;
        if is_final {
            break;
        }
    }
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

#[gpui_kit::test]
fn combine_and_four_mode_join_with_open_tab_lease_work_through_gpui_ui(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let part_a = dir.path().join("part_a.csv");
    let part_b = dir.path().join("part_b.csv");
    std::fs::write(&part_a, "id,k2,name\n1,a,Alice\n2,b,Bob\n").unwrap();
    std::fs::write(&part_b, "id,k2,name\n3,c,Cara\n").unwrap();

    let extra_path = dir.path().join("extra.csv");
    std::fs::write(&extra_path, "id,k2,score\n1,a,95\n4,d,80\n").unwrap();

    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    // 1. Combine Files dialog via welcome button -> check -> reject .gz -> write combined CSV.
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("welcome-combine").visible());
        assert!(window.find("welcome-join").visible());
        window.click("welcome-combine", cx);
        window.render_frame(cx);
        assert!(window.find("combine-check").visible());
    })
    .unwrap();

    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.combine_add_paths(vec![part_a.clone(), part_b.clone()], cx);
            c.combine_start_check(cx);
        });
    });
    wait_for_ui(cx, handle, &controller, &view, |_, _, cx| {
        let wf = controller.read(cx).combine();
        !wf.busy() && wf.plan.is_some()
    });

    // Reject .gz destination early.
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.combine_prepare_destination(dir.path().join("bad.csv.gz"), cx);
        });
    });
    assert!(cx.update(|cx| controller.read(cx).combine().destination.is_none()));

    let combined_out = dir.path().join("combined.csv");
    let sub_dir = dir.path().join("sub");
    std::fs::create_dir_all(&sub_dir).unwrap();
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.combine_prepare_destination(combined_out.clone(), cx);
        });
    });
    // Non-existent alias `/dir/sub/../combined.csv` must be detected as reserved before creation.
    assert!(cx.update(|cx| {
        controller
            .read(cx)
            .path_reserved(&sub_dir.join("../combined.csv"))
    }));
    wait_for_ui(cx, handle, &controller, &view, |_, _, cx| {
        let wf = controller.read(cx).combine();
        !wf.busy() && wf.space.is_some()
    });
    cx.update(|cx| {
        controller.update(cx, |c, cx| c.combine_start_write(cx));
    });
    wait_for_ui(cx, handle, &controller, &view, |_, _, cx| {
        let wf = controller.read(cx).combine();
        !wf.busy() && wf.result.is_some()
    });
    assert_eq!(
        std::fs::read_to_string(&combined_out).unwrap(),
        "id,k2,name\n1,a,Alice\n2,b,Bob\n3,c,Cara\n"
    );

    cx.update_window(handle.into(), |_, window, cx| {
        window.click("combine-close", cx);
        window.render_frame(cx);
    })
    .unwrap();

    // 2. Open part_a in a tab, open Join Files, bind open tab -> verify DocumentLease blocks close.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![part_a.clone()],
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
    let doc_id = cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .active_document(win_id)
            .unwrap()
    });

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| q.open_join_dialog_now(window, cx));
        window.render_frame(cx);
        assert!(window.find("join-preview").visible());
    })
    .unwrap();

    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.join_set_open_tab_side(0, doc_id, cx).unwrap();
            c.join_set_file_side(1, extra_path.clone(), cx).unwrap();
        });
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        let wf = controller.read(cx).join();
        !wf.busy()
            && wf.headers[0].is_some()
            && wf.headers[1].is_some()
            && q.snapshot(cx).is_some_and(|i| i.join_reserved)
    });

    // Closing the leased tab must be rejected while Join holds its DocumentLease.
    let close_attempt = cx
        .update(|cx| controller.update(cx, |c, cx| c.request_close_tab(win_id, doc_id, false, cx)));
    assert!(matches!(close_attempt, CloseReviewAction::Wait { .. }));

    // Configure composite key (id, k2) and verify all 4 join modes.
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.join_add_key_pair(cx);
            c.join_set_key_column(0, 0, 0, cx);
            c.join_set_key_column(0, 1, 0, cx);
            c.join_set_key_column(1, 0, 1, cx);
            c.join_set_key_column(1, 1, 1, cx);
        });
    });

    for (mode, expected_rows) in [
        (quarry_core::JoinMode::Inner, 1u64),
        (quarry_core::JoinMode::Left, 2),
        (quarry_core::JoinMode::Right, 2),
        (quarry_core::JoinMode::Full, 3),
    ] {
        cx.update(|cx| {
            controller.update(cx, |c, cx| {
                c.join_set_mode(mode, cx);
                c.join_start_analysis(cx);
            });
        });
        wait_for_ui(cx, handle, &controller, &view, |_, _, cx| {
            let wf = controller.read(cx).join();
            !wf.busy() && wf.plan.is_some()
        });
        let rows = cx.update(|cx| {
            controller
                .read(cx)
                .join()
                .plan
                .as_ref()
                .unwrap()
                .analysis()
                .output_rows
        });
        assert_eq!(rows, expected_rows, "mode {mode:?}");
    }

    let joined_out = dir.path().join("joined.csv");
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.join_prepare_destination(joined_out.clone(), cx).unwrap();
            c.join_start_write(cx).unwrap();
        });
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        let wf = controller.read(cx).join();
        !wf.busy() && wf.result.is_some() && q.snapshot(cx).is_some_and(|i| !i.join_reserved)
    });
    assert!(joined_out.exists());

    cx.update_window(handle.into(), |_, window, cx| {
        window.click("join-close", cx);
        window.render_frame(cx);
    })
    .unwrap();
    let close_after = cx
        .update(|cx| controller.update(cx, |c, cx| c.request_close_tab(win_id, doc_id, false, cx)));
    assert_eq!(close_after, CloseReviewAction::Proceed);
}

#[gpui_kit::test]
fn gzip_streaming_lifecycle_preview_decompress_and_save_as_uncompressed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let gz_path = dir.path().join("sales.csv.gz");
    let payload = b"id,amount\n1,100\n2,200\n3,300\n";
    std::fs::write(&gz_path, encode_stored_gzip(payload)).unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::apply_light_theme(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        WorkbenchController::reset_for_test(
            cx,
            WorkbenchSchedulerConfig {
                working_directory: scratch.path().to_path_buf(),
                custom_working_directory: true,
                index_cache_enabled: false,
                defer_gzip_decompression: true,
                ..WorkbenchSchedulerConfig::default()
            },
        )
    });
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![gz_path.clone()],
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                None,
                window,
                cx,
            );
        });
        window.render_frame(cx);
        assert!(window.find("gzip-start-decompress").visible());
        assert!(window.find("gzip-preview-only").visible());
        // Switch to Preview Only first.
        window.click("gzip-preview-only", cx);
        window.render_frame(cx);
        assert!(window.find("gzip-preview-reload").visible());
        // Reload from Disk must respect the shared two-writer cap.
        {
            let slots = controller
                .read(cx)
                .scheduler()
                .decompression_slots()
                .clone();
            let _s1 = slots.claim();
            let _s2 = slots.claim();
            let blocked = controller.update(cx, |c, cx| c.reload_active_tab(win_id, cx));
            assert!(
                blocked.is_err(),
                "reloading a compressed tab must fail while both write slots are occupied"
            );
        }
        // Reload from Disk triggers full decompression once slots are free.
        window.click("gzip-preview-reload", cx);
    })
    .unwrap();

    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| {
            i.is_compressed && !i.compressed_pending && !i.compressed_preview && i.index_done
        })
    });

    // Compressed documents are read-only (Save disabled) but Save As is enabled,
    // and retained_storage_paths() includes the private decompressed copy.
    cx.update(|cx| {
        let q = view.read(cx);
        assert!(!q.command_enabled(quarry_workbench::CommandId::Save, cx));
        assert!(q.command_enabled(quarry_workbench::CommandId::SaveAs, cx));
        let retained = controller.read(cx).scheduler().retained_storage_paths();
        assert!(
            !retained.is_empty(),
            "completed gzip private copy must be counted in retained_storage_paths()"
        );
    });

    // Reject .gz Save As destination, then write uncompressed copy.
    let bad_save = cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.start_save_active(win_id, None, Some(dir.path().join("out.csv.gz")), None, cx)
        })
    });
    assert!(bad_save.is_err());

    let uncompressed_copy = dir.path().join("sales.csv");
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_save_active(win_id, None, Some(uncompressed_copy.clone()), None, cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| !i.saving) && uncompressed_copy.exists()
    });
    assert_eq!(
        std::fs::read_to_string(&uncompressed_copy).unwrap(),
        "id,amount\n1,100\n2,200\n3,300\n"
    );
}

#[gpui_kit::test]
fn inspect_recovery_temporary_storage_and_about_dialog_work_through_gpui_ui(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let uneven = dir.path().join("uneven.csv");
    std::fs::write(&uneven, "a,b\n1,2\n3,4,5\n6,7\n").unwrap();
    let pref_file = dir.path().join("prefs/storage-preferences");
    let custom_scratch = dir.path().join("custom-scratch");
    std::fs::create_dir_all(&custom_scratch).unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::apply_light_theme(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        WorkbenchController::reset_for_test_with_prefs(
            cx,
            WorkbenchSchedulerConfig {
                index_cache_enabled: false,
                ..WorkbenchSchedulerConfig::default()
            },
            quarry_workbench::StoragePreferences::load_from(Some(pref_file.clone()), true),
        )
    });
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![uneven.clone()],
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

    // 1. Inspect File & Recovery dialog -> inspect -> select eligible -> jump & recover.
    let (second_handle, second_view) = open_test_window(cx, &controller);
    let second_win_id = cx.update(|cx| second_view.read(cx).window_id);

    // Multi-file open failure on an empty window must preserve the failed file path and options
    // so the user can launch Inspect File directly from the error banner.
    let missing_batch_file = dir.path().join("missing-first.csv");
    let batch_opts = OpenOptions {
        header_mode: HeaderMode::FirstRow,
        ..OpenOptions::default()
    };
    let batch_err = cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.open_paths(
                second_win_id,
                vec![missing_batch_file.clone(), uneven.clone()],
                batch_opts,
                Some(quarry_core::Encoding::Utf8),
                cx,
            )
            .unwrap_err()
        })
    });
    let failed = batch_err
        .inspection_target
        .as_ref()
        .expect("multi-file open failure should retain inspection_target");
    assert_eq!(failed.path, missing_batch_file);
    assert_eq!(failed.encoding, Some(quarry_core::Encoding::Utf8));
    assert_eq!(failed.options.header_mode, HeaderMode::FirstRow);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| q.open_inspect_dialog_now(window, cx));
        window.render_frame(cx);
        assert!(window.find("inspect-run").visible());
        window.click("inspect-run", cx);
    })
    .unwrap();
    // Opening Inspect from a second window while Window 1 owns it must NOT steal/reconfigure it.
    cx.update_window(second_handle.into(), |_, window, cx| {
        second_view.update(cx, |q, cx| q.open_inspect_dialog_now(window, cx));
        assert!(!second_view.read(cx).editing);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |_, _, cx| {
        let wf = controller.read(cx).inspect();
        !wf.busy() && wf.report.as_ref().is_some_and(|r| !r.findings.is_empty())
    });

    let recovered = dir.path().join("recovered.csv");
    let sub_dir = dir.path().join("sub");
    std::fs::create_dir_all(&sub_dir).unwrap();
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.inspect_select_all_eligible(cx);
            assert!(c.inspect_jump_to_finding(win_id, 0, cx));
            // Attempting recovery to an alias of the open source file must be blocked.
            let open_alias = sub_dir.join("..").join("uneven.csv");
            assert!(c.inspect_start_recovery(open_alias, cx).is_err());
            c.inspect_start_recovery(recovered.clone(), cx).unwrap();
        });
    });
    wait_for_ui(cx, handle, &controller, &view, |_, _, cx| {
        let wf = controller.read(cx).inspect();
        !wf.busy() && wf.recovery_result.is_some()
    });
    assert_eq!(
        std::fs::read_to_string(&recovered).unwrap(),
        "a,b\n1,2\n6,7\n"
    );

    // Changing parsing options must invalidate the completed report so stale recovery cannot run.
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.inspect_set_options(
                quarry_workbench::DelimiterMode::Tab,
                HeaderMode::FirstRow,
                None,
                cx,
            );
            assert!(c.inspect().report.is_none());
            assert!(c.inspect().selected.is_empty());
        });
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window.click("inspect-close", cx);
        window.render_frame(cx);
    })
    .unwrap();

    // 2. Temporary Storage & Index Cache dialog -> set custom folder (persisted) -> toggle cache.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.temporary_storage_action(&crate::menus::TemporaryStorage, window, cx)
        });
        window.render_frame(cx);
        assert!(window.find("storage-choose-folder").visible());
        assert!(window.find("storage-clear-cache").visible());
    })
    .unwrap();
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.set_scratch_directory(custom_scratch.clone(), cx).unwrap();
            c.set_index_cache_enabled(false, cx).unwrap();
            assert!(!c.index_cache_enabled());
        });
    });
    let reloaded = quarry_workbench::StoragePreferences::load_from(Some(pref_file), true);
    assert_eq!(
        reloaded.scratch_directory.as_deref(),
        Some(custom_scratch.as_path())
    );
    assert_eq!(reloaded.index_cache_enabled, Some(false));
    assert!(!reloaded.index_cache_enabled());
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.reset_scratch_directory(cx).unwrap();
            assert!(!c.custom_working_directory());
        });
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("storage-close", cx);
        window.render_frame(cx);
    })
    .unwrap();

    // 3. About Quarry dialog -> verify title and Copy Build Details.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.about_quarry_action(&crate::menus::AboutQuarry, window, cx)
        });
        window.render_frame(cx);
        assert!(window.find("about-copy-details").visible());
        window.click("about-copy-details", cx);
        window.click("about-close", cx);
        window.render_frame(cx);
    })
    .unwrap();
}

#[gpui_kit::test]
fn themes_dark_omarchy_persistence_and_tabular_figures_work_through_gpui_ui(
    cx: &mut gpui_kit::TestAppContext,
) {
    // 1. Verify OpenType `tnum` tabular figures are configured for grid cells.
    let features = crate::grid::tabular_font_features();
    assert_eq!(features.tag_value_list(), &[("tnum".to_owned(), 1)]);

    let home = tempfile::tempdir().unwrap();
    let pref_file = home.path().join("quarry-config/theme-preferences");
    let omarchy_dir = home.path().join(".local/state/omarchy/current/theme");
    std::fs::create_dir_all(&omarchy_dir).unwrap();
    std::fs::write(
        home.path().join(".local/state/omarchy/current/theme.name"),
        "tokyo-night\n",
    )
    .unwrap();
    std::fs::write(
        home.path()
            .join(".local/state/omarchy/current/theme.generation"),
        "gen-1\n",
    )
    .unwrap();
    std::fs::write(
        omarchy_dir.join("colors.toml"),
        r##"
mode = "dark"
accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"
background = "#1a1b26"
dark_background = "#13141c"
lighter_background = "#24283b"
foreground = "#a9b1d6"
dark_foreground = "#565f89"
bright_foreground = "#c0caf5"
red = "#f7768e"
yellow = "#e0af68"
green = "#9ece6a"
cyan = "#449dab"
"##,
    )
    .unwrap();

    let csv_dir = tempfile::tempdir().unwrap();
    let sample = csv_dir.path().join("numbers.csv");
    std::fs::write(
        &sample,
        "item,amount,pct\nalpha,1234.50,12.5%\nbeta,9.00,100%\n",
    )
    .unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::apply_light_theme(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        WorkbenchController::reset_for_test_with_theme(
            cx,
            WorkbenchSchedulerConfig {
                index_cache_enabled: false,
                ..WorkbenchSchedulerConfig::default()
            },
            quarry_workbench::ThemePreferences::load_from(Some(pref_file.clone()), true),
            Some(home.path().to_path_buf()),
        )
    });
    let (handle, view) = open_test_window(cx, &controller);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![sample.clone()],
                OpenOptions::default(),
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

    // 2. Switch to Dark mode via action and verify GPUI ThemeMode, persisted preference, and
    //    that an existing active file-operation notice is never overwritten by theme changes.
    let win_id = cx.update(|cx| view.read(cx).window_id);
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.set_window_message(
                win_id,
                Some(quarry_workbench::AppMessage::error(
                    "Save failed: disk full",
                )),
                cx,
            );
        });
    });
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.theme_dark_action(&crate::menus::ThemeDark, window, cx)
        });
        window.render_frame(cx);
        assert!(gpui_kit::component::Theme::global(cx).mode.is_dark());
        assert_eq!(controller.read(cx).active_palette(), quarry_workbench::DARK);
        assert_eq!(
            gpui_kit::component::Theme::global(cx).colors.danger,
            gpui_kit::rgb(quarry_workbench::DARK.error_text.to_rgb_hex()).into()
        );
        assert_eq!(
            controller
                .read(cx)
                .window_message(win_id)
                .map(|m| m.text.as_str()),
            Some("Save failed: disk full"),
            "theme switching must not overwrite file-operation notices"
        );
    })
    .unwrap();
    let persisted = quarry_workbench::ThemePreferences::load_from(Some(pref_file.clone()), true);
    assert_eq!(
        persisted.preference,
        quarry_workbench::ThemePreference::Dark
    );

    // 3. Switch to Light mode via action.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.theme_light_action(&crate::menus::ThemeLight, window, cx)
        });
        window.render_frame(cx);
        assert!(!gpui_kit::component::Theme::global(cx).mode.is_dark());
        assert_eq!(
            controller.read(cx).active_palette(),
            quarry_workbench::LIGHT
        );
    })
    .unwrap();

    // 4. Switch to Omarchy sync and verify live Tokyo Night palette + live generation reload.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.theme_omarchy_action(&crate::menus::ThemeOmarchy, window, cx)
        });
        window.render_frame(cx);
        assert!(gpui_kit::component::Theme::global(cx).mode.is_dark());
        assert_eq!(
            controller.read(cx).resolved_theme().source_label,
            "Omarchy (tokyo-night)"
        );
        assert_eq!(
            controller.read(cx).active_palette().canvas,
            quarry_workbench::Rgba::rgb(0x1a, 0x1b, 0x26)
        );
    })
    .unwrap();

    // Update Omarchy generation + light theme on disk and verify live reload detects it.
    std::fs::write(
        home.path().join(".local/state/omarchy/current/theme.name"),
        "rose-pine-dawn\n",
    )
    .unwrap();
    std::fs::write(
        home.path()
            .join(".local/state/omarchy/current/theme.generation"),
        "gen-2\n",
    )
    .unwrap();
    std::fs::write(
        omarchy_dir.join("colors.toml"),
        "mode = \"light\"\nbackground = \"#faf4ed\"\nforeground = \"#575279\"\naccent = \"#286983\"\n",
    )
    .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        let updated = controller
            .update(cx, |c, cx| c.reload_omarchy_theme_now(cx))
            .expect("omarchy generation change should trigger reload");
        crate::apply_resolved_theme(&updated, cx);
        window.render_frame(cx);
        assert!(!gpui_kit::component::Theme::global(cx).mode.is_dark());
        assert_eq!(updated.source_label, "Omarchy (rose-pine-dawn)");
        assert!(quarry_workbench::palette_meets_wcag_aa(&updated.palette));
    })
    .unwrap();

    // If a new generation is observed while colors.toml is temporarily missing, fallback occurs
    // without caching that generation, so the next poll recovers once colors.toml reappears.
    std::fs::remove_file(omarchy_dir.join("colors.toml")).unwrap();
    std::fs::write(
        home.path()
            .join(".local/state/omarchy/current/theme.generation"),
        "gen-3\n",
    )
    .unwrap();
    cx.update(|cx| {
        let fallback = controller
            .update(cx, |c, cx| c.reload_omarchy_theme_now(cx))
            .expect("missing colors.toml should transition to fallback");
        assert!(fallback.warning.is_some());
        assert!(controller.read(cx).theme_warning().is_some());
    });
    std::fs::write(
        omarchy_dir.join("colors.toml"),
        "mode = \"dark\"\nbackground = \"#1a1b26\"\nforeground = \"#c0caf5\"\naccent = \"#7aa2f7\"\n",
    )
    .unwrap();
    cx.update(|cx| {
        let recovered = controller
            .update(cx, |c, cx| c.reload_omarchy_theme_now(cx))
            .expect("subsequent reload should recover once colors.toml is ready");
        assert!(recovered.warning.is_none());
        assert!(
            controller.read(cx).theme_warning().is_none(),
            "Omarchy fallback warning must clear automatically when sync recovers"
        );
        assert_eq!(
            recovered.palette.canvas,
            quarry_workbench::Rgba::rgb(0x1a, 0x1b, 0x26)
        );
    });
}

#[gpui_kit::test]
fn two_hundred_tabs_across_four_windows_stay_bounded_and_preserve_dirty_work(
    cx: &mut gpui_kit::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = Vec::with_capacity(200);
    for idx in 0..200 {
        let p = dir.path().join(format!("table_{idx:03}.csv"));
        std::fs::write(
            &p,
            format!(
                "id,category,amount\n1,alpha,{}\n2,beta,{}\n",
                idx * 10,
                idx * 20
            ),
        )
        .unwrap();
        paths.push(p);
    }

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::apply_light_theme(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        WorkbenchController::reset_for_test(
            cx,
            WorkbenchSchedulerConfig {
                index_cache_enabled: false,
                ..WorkbenchSchedulerConfig::default()
            },
        )
    });

    // Open 4 windows and distribute 50 files lazily into each window (200 total tabs).
    let mut windows = Vec::new();
    for win_idx in 0..4 {
        let (handle, view) = open_test_window(cx, &controller);
        let chunk = paths[win_idx * 50..(win_idx + 1) * 50].to_vec();
        cx.update_window(handle.into(), |_, window, cx| {
            view.update(cx, |q, cx| {
                q.open_paths_direct(chunk, OpenOptions::default(), None, window, cx);
            });
        })
        .unwrap();
        windows.push((handle, view));
    }

    for (handle, view) in &windows {
        wait_for_ui(cx, *handle, &controller, view, |q, _, cx| {
            q.snapshot(cx).is_some_and(|i| i.index_done)
        });
    }

    // Verify 200 total tabs, >= 1 hydrated tab in EVERY window simultaneously, and FD budget < 128.
    let window_ids: Vec<_> = windows
        .iter()
        .map(|(_, view)| cx.update(|cx| view.read(cx).window_id))
        .collect();
    cx.update(|cx| {
        let sched = controller.read(cx).scheduler();
        assert_eq!(sched.total_tabs(), 200);
        assert!(sched.hydrated_count() >= 4);
        assert!(sched.hydrated_count() <= 8);
        for &wid in &window_ids {
            assert!(
                sched.tabs_in_window(wid).into_iter().any(|id| {
                    sched.residency(id) == Some(quarry_workbench::ResidencyState::Hydrated)
                }),
                "window {wid:?} must keep at least one hydrated tab"
            );
        }
        assert!(sched.fd_budget().snapshot().peak_reserved < 128);
        assert!(sched.fd_budget().snapshot().total_reserved < 128);
    });

    // Make one tab in Window 1 dirty, switch through 12 other tabs in Window 1 to trigger
    // dormancy eviction, then switch back and confirm the unsaved edit survived intact.
    let (w1_handle, w1_view) = windows[0].clone();
    let w1_id = cx.update(|cx| w1_view.read(cx).window_id);
    let w1_tabs = cx.update(|cx| controller.read(cx).scheduler().tabs_in_window(w1_id));
    assert_eq!(w1_tabs.len(), 50);
    let dirty_doc = w1_tabs[0];
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.apply_cell_edit(w1_id, dirty_doc, 1, 1, "EDITED_200".into(), cx)
                .unwrap();
        });
    });
    for &tab_id in &w1_tabs[1..12] {
        cx.update(|cx| {
            controller.update(cx, |c, cx| {
                c.activate_tab(w1_id, tab_id, cx).unwrap();
            });
        });
        wait_for_ui(cx, w1_handle, &controller, &w1_view, |q, _, cx| {
            q.snapshot(cx)
                .is_some_and(|i| i.document_id == tab_id && i.index_done)
        });
    }
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.activate_tab(w1_id, dirty_doc, cx).unwrap();
        });
    });
    wait_for_ui(cx, w1_handle, &controller, &w1_view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.document_id == dirty_doc && i.dirty && i.index_done)
    });
    cx.update(|cx| {
        let sched = controller.read(cx).scheduler();
        assert_eq!(sched.total_tabs(), 200);
        assert!(sched.fd_budget().snapshot().peak_reserved < 128);
        let tab = sched.tab(dirty_doc).unwrap();
        let doc = tab.document.as_ref().unwrap();
        assert!(doc.is_dirty());
        assert_eq!(
            doc.cell_edits.get(&(1, 1)).map(|v| v.as_slice()),
            Some(b"EDITED_200".as_slice())
        );
    });
}
