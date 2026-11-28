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
                max_hydrated_tabs: 4,
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
        let sched = controller.read(cx).scheduler();
        assert_eq!(
            sched.residency(dirty_doc),
            Some(quarry_workbench::ResidencyState::DormantDirty),
            "dirty tab must have been evicted to DormantDirty before wake verification"
        );
    });
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

#[test]
fn file_url_and_launch_arg_parsing_handles_finder_paths_and_rejects_remote_urls() {
    use std::ffi::OsString;
    use std::path::PathBuf;

    assert_eq!(
        crate::file_url_to_path("file:///Users/test/My%20Data%20%231.csv"),
        Some(PathBuf::from("/Users/test/My Data #1.csv"))
    );
    assert_eq!(
        crate::file_url_to_path("FILE://LOCALHOST/tmp/caf%C3%A9.csv?ignored=1#frag"),
        Some(PathBuf::from("/tmp/café.csv"))
    );
    assert_eq!(
        crate::file_url_to_path("https://example.com/data.csv"),
        None
    );
    assert_eq!(
        crate::file_url_to_path("file://evil.example/data.csv"),
        None
    );
    assert_eq!(crate::file_url_to_path("file://aééééé/data.csv"), None);
    assert_eq!(crate::file_url_to_path("ééééééé"), None);
    assert_eq!(crate::file_url_to_path("file:///tmp/bad%2"), None);
    assert_eq!(crate::file_url_to_path("file:///tmp/bad%ZZ.csv"), None);
    assert_eq!(crate::file_url_to_path("file:///tmp/null%00.csv"), None);

    let filtered = crate::filter_launch_paths([
        OsString::from("-psn_0_12345"),
        OsString::from("/tmp/first.csv"),
        OsString::from("/tmp/second.csv"),
    ]);
    assert_eq!(
        filtered,
        vec![
            PathBuf::from("/tmp/first.csv"),
            PathBuf::from("/tmp/second.csv"),
        ]
    );
}

#[test]
fn about_dialog_info_plist_parser_reads_quarry_metadata_and_rejects_foreign_bundles() {
    let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>io.github.danchamorro.quarry</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>214</string>
    <key>QuarryArchitecture</key>
    <string>arm64</string>
    <key>QuarryGitRevision</key>
    <string>b3afd22baf1a89d23505e0b4cf6ab4b6f2611dfc</string>
    <key>QuarrySourceStatus</key>
    <string>clean</string>
</dict>
</plist>"#;
    let info = crate::workflows_ui::parse_info_plist(plist, "io.github.danchamorro.quarry")
        .expect("matching bundle id must parse");
    assert_eq!(info.version.as_deref(), Some("0.1.0"));
    assert_eq!(info.build.as_deref(), Some("214"));
    assert_eq!(info.architecture.as_deref(), Some("arm64"));
    assert_eq!(
        info.revision.as_deref(),
        Some("b3afd22baf1a89d23505e0b4cf6ab4b6f2611dfc")
    );
    assert_eq!(info.source_status.as_deref(), Some("clean"));
    let formatted = crate::workflows_ui::format_build_details(&info);
    assert!(formatted.contains("Quarry 0.1.0"));
    assert!(formatted.contains("Build: 214"));
    assert!(formatted.contains("Revision: b3afd22baf1a89d23505e0b4cf6ab4b6f2611dfc"));
    assert!(formatted.contains("Source status: clean"));
    assert!(!formatted.contains("GPUI preview"));

    let foreign = plist.replace("io.github.danchamorro.quarry", "com.example.other");
    assert!(
        crate::workflows_ui::parse_info_plist(&foreign, "io.github.danchamorro.quarry").is_none()
    );
}

#[gpui_kit::test]
fn two_dirty_docs_in_two_windows_close_original_edit_and_save_survivor(
    cx: &mut gpui_kit::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("alpha.csv");
    let path_b = dir.path().join("beta.csv");
    std::fs::write(&path_a, "id,val\n1,orig_a\n").unwrap();
    std::fs::write(&path_b, "id,val\n1,orig_b\n").unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
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

    let (w1_handle, w1_view) = open_test_window(cx, &controller);
    let (w2_handle, w2_view) = open_test_window(cx, &controller);
    let w1_id = cx.update(|cx| w1_view.read(cx).window_id);
    let w2_id = cx.update(|cx| w2_view.read(cx).window_id);

    cx.update_window(w1_handle.into(), |_, window, cx| {
        w1_view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path_a.clone()],
                OpenOptions::default(),
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    cx.update_window(w2_handle.into(), |_, window, cx| {
        w2_view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path_b.clone()],
                OpenOptions::default(),
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, w1_handle, &controller, &w1_view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.index_done)
    });
    wait_for_ui(cx, w2_handle, &controller, &w2_view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.index_done)
    });

    let doc_a = cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .active_document(w1_id)
            .unwrap()
    });
    let doc_b = cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .active_document(w2_id)
            .unwrap()
    });

    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.apply_cell_edit(w1_id, doc_a, 1, 1, "dirty_a".into(), cx)
                .unwrap();
            c.apply_cell_edit(w2_id, doc_b, 1, 1, "dirty_b_1".into(), cx)
                .unwrap();
        });
    });

    // Request closing Window 1 -> prompts for doc_a -> Discard closes Window 1 cleanly.
    let review = cx.update(|cx| controller.update(cx, |c, cx| c.request_close_window(w1_id, cx)));
    let review_token = match review {
        crate::workbench::CloseReviewAction::ConfirmTab {
            review_token,
            window_id,
            document_id,
            ..
        } => {
            assert_eq!(window_id, w1_id);
            assert_eq!(document_id, doc_a);
            review_token
        }
        other => panic!("expected ConfirmTab for dirty doc_a, got {other:?}"),
    };
    let next = cx
        .update(|cx| controller.update(cx, |c, cx| c.approve_discard_in_review(review_token, cx)));
    assert!(matches!(next, crate::workbench::CloseReviewAction::Proceed));
    // `sync_from_controller` reacts to `cx.notify()` and closes Window 1 cleanly.
    cx.update(|cx| {
        assert!(
            controller.read(cx).window_handle(w1_id).is_none(),
            "Window 1 handle must be unregistered once closed"
        );
        assert!(
            controller
                .read(cx)
                .scheduler()
                .tabs_in_window(w1_id)
                .is_empty()
        );
        assert_eq!(
            controller.read(cx).scheduler().tabs_in_window(w2_id),
            vec![doc_b]
        );
    });

    // Survivor Window 2 still owns dirty doc_b; make another edit and save it to disk.
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.apply_cell_edit(w2_id, doc_b, 1, 1, "dirty_b_final".into(), cx)
                .unwrap();
            c.start_save_active(w2_id, Some(doc_b), None, None, cx)
                .unwrap();
        });
    });
    wait_for_ui(cx, w2_handle, &controller, &w2_view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| !i.saving && !i.dirty)
    });

    assert_eq!(
        std::fs::read_to_string(&path_a).unwrap(),
        "id,val\n1,orig_a\n"
    );
    assert_eq!(
        std::fs::read_to_string(&path_b).unwrap(),
        "id,val\n1,dirty_b_final\n"
    );
}

#[gpui_kit::test]
fn quit_cancelled_at_second_dirty_prompt_preserves_survivor_work_and_undo_history(
    cx: &mut gpui_kit::TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("first.csv");
    let path_b = dir.path().join("second.csv");
    std::fs::write(&path_a, "id,val\n1,a0\n").unwrap();
    std::fs::write(&path_b, "id,val\n1,b0\n").unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
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

    let (w1_handle, w1_view) = open_test_window(cx, &controller);
    let (w2_handle, w2_view) = open_test_window(cx, &controller);
    let w1_id = cx.update(|cx| w1_view.read(cx).window_id);
    let w2_id = cx.update(|cx| w2_view.read(cx).window_id);

    cx.update_window(w1_handle.into(), |_, window, cx| {
        w1_view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path_a.clone()],
                OpenOptions::default(),
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    cx.update_window(w2_handle.into(), |_, window, cx| {
        w2_view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path_b.clone()],
                OpenOptions::default(),
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, w1_handle, &controller, &w1_view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.index_done)
    });
    wait_for_ui(cx, w2_handle, &controller, &w2_view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.index_done)
    });

    let doc_a = cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .active_document(w1_id)
            .unwrap()
    });
    let doc_b = cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .active_document(w2_id)
            .unwrap()
    });
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.apply_cell_edit(w1_id, doc_a, 1, 1, "a1".into(), cx)
                .unwrap();
            c.apply_cell_edit(w2_id, doc_b, 1, 1, "b1".into(), cx)
                .unwrap();
            c.apply_cell_edit(w2_id, doc_b, 1, 1, "b2".into(), cx)
                .unwrap();
        });
    });

    // Cmd+Q starts unified cross-window quit review -> prompts on first dirty doc.
    let first_prompt = cx.update(|cx| controller.update(cx, |c, cx| c.request_quit(cx)));
    let (token_1, doc_1) = match first_prompt {
        crate::workbench::CloseReviewAction::ConfirmTab {
            review_token,
            document_id,
            ..
        } => (review_token, document_id),
        other => panic!("expected first ConfirmTab during quit, got {other:?}"),
    };
    let expected_survivor = if doc_1 == doc_a { doc_b } else { doc_a };
    let survivor_window = if expected_survivor == doc_b {
        w2_id
    } else {
        w1_id
    };

    // Discard the first dirty doc -> review advances to the second dirty doc.
    let second_prompt =
        cx.update(|cx| controller.update(cx, |c, cx| c.approve_discard_in_review(token_1, cx)));
    let (token_2, doc_2) = match second_prompt {
        crate::workbench::CloseReviewAction::ConfirmTab {
            review_token,
            document_id,
            ..
        } => (review_token, document_id),
        other => panic!("expected second ConfirmTab during quit, got {other:?}"),
    };
    assert_eq!(doc_2, expected_survivor);

    // Cancel ("Keep Editing") at the second dirty prompt!
    cx.update(|cx| {
        controller.update(cx, |c, cx| c.cancel_close_review(Some(token_2), cx));
    });

    cx.update(|cx| {
        assert!(!controller.update(cx, |c, _| c.take_quit_complete()));
        let sched = controller.read(cx).scheduler();
        let tab = sched
            .tab(expected_survivor)
            .expect("survivor tab must remain open");
        let doc = tab
            .document
            .as_ref()
            .expect("survivor doc must stay intact");
        assert!(doc.is_dirty());
        if expected_survivor == doc_b {
            assert_eq!(
                doc.cell_edits.get(&(1, 1)).map(|v| v.as_slice()),
                Some(b"b2".as_slice())
            );
        }
    });

    // Verify Undo & Redo history survived intact on doc_b (the second dirty doc).
    if expected_survivor == doc_b {
        cx.update(|cx| {
            // `redo = false` -> Undo restores edit 1 (`b1`).
            controller
                .update(cx, |c, cx| c.undo_redo_active(survivor_window, false, cx))
                .unwrap();
            let doc = controller
                .read(cx)
                .scheduler()
                .tab(doc_b)
                .unwrap()
                .document
                .as_ref()
                .unwrap();
            assert_eq!(
                doc.cell_edits.get(&(1, 1)).map(|v| v.as_slice()),
                Some(b"b1".as_slice())
            );
            // `redo = true` -> Redo restores edit 2 (`b2`).
            controller
                .update(cx, |c, cx| c.undo_redo_active(survivor_window, true, cx))
                .unwrap();
            let doc = controller
                .read(cx)
                .scheduler()
                .tab(doc_b)
                .unwrap()
                .document
                .as_ref()
                .unwrap();
            assert_eq!(
                doc.cell_edits.get(&(1, 1)).map(|v| v.as_slice()),
                Some(b"b2".as_slice())
            );
        });
    }
}

#[gpui_kit::test]
fn save_as_targets_originating_document_across_focus_change(cx: &mut gpui_kit::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("source_a.csv");
    let path_b = dir.path().join("source_b.csv");
    let path_c = dir.path().join("source_c.csv");
    let copy_a = dir.path().join("source_a-copy.csv");
    let copy_wrong = dir.path().join("source_a-wrong.csv");
    std::fs::write(&path_a, "id,val\n1,alpha\n").unwrap();
    std::fs::write(&path_b, "id,val\n1,beta\n").unwrap();
    std::fs::write(&path_c, "id,val\n1,gamma\n").unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
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

    let (w1_handle, w1_view) = open_test_window(cx, &controller);
    let (w2_handle, w2_view) = open_test_window(cx, &controller);
    let w1_id = cx.update(|cx| w1_view.read(cx).window_id);
    let w2_id = cx.update(|cx| w2_view.read(cx).window_id);
    cx.update_window(w1_handle.into(), |_, window, cx| {
        w1_view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path_a.clone(), path_c.clone()],
                OpenOptions::default(),
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    cx.update_window(w2_handle.into(), |_, window, cx| {
        w2_view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path_b.clone()],
                OpenOptions::default(),
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    let w1_tabs = cx.update(|cx| controller.read(cx).scheduler().tabs_in_window(w1_id));
    assert_eq!(w1_tabs.len(), 2);
    let (doc_a, doc_c) = (w1_tabs[0], w1_tabs[1]);

    cx.update(|cx| {
        controller.update(cx, |c, cx| c.activate_tab(w1_id, doc_a, cx).unwrap());
    });
    wait_for_ui(cx, w1_handle, &controller, &w1_view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.document_id == doc_a && i.index_done)
    });
    wait_for_ui(cx, w2_handle, &controller, &w2_view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.index_done)
    });
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.apply_cell_edit(w1_id, doc_a, 1, 1, "ALPHA_EDITED".into(), cx)
                .unwrap();
        });
    });

    // 1. Open the real Save As picker on Window 1 (`doc_a`), shift OS focus to Window 2
    // while the picker is still pending, then complete the picker and verify the callback
    // writes `doc_a`'s edited rows (not Window 2's document).
    cx.update_window(w1_handle.into(), |_, window, cx| {
        w1_view.update(cx, |q, cx| q.choose_save_destination(None, window, cx));
    })
    .unwrap();
    assert!(
        cx.did_prompt_for_new_path(),
        "choose_save_destination must invoke the native new-path picker"
    );
    cx.update_window(w2_handle.into(), |_, window, _| window.activate_window())
        .unwrap();
    let doc_b = cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .active_document(w2_id)
            .unwrap()
    });
    assert_ne!(doc_b, doc_a);
    let copy_a_for_picker = copy_a.clone();
    cx.simulate_new_path_selection(move |_| Some(copy_a_for_picker));
    wait_for_ui(cx, w1_handle, &controller, &w1_view, |q, _, cx| {
        !q.confirming
            && controller
                .read(cx)
                .scheduler()
                .tab(doc_a)
                .is_some_and(|t| !t.document_writing())
            && copy_a.exists()
    });
    assert_eq!(
        std::fs::read_to_string(&copy_a).unwrap(),
        "id,val\n1,ALPHA_EDITED\n"
    );

    // 2. Open the real Save As picker on Window 1 (`doc_a`), switch Window 1's active tab to
    // `doc_c` before the picker resolves, and verify the picker callback rejects cleanly
    // instead of writing `doc_c` to `doc_a`'s chosen destination.
    cx.update_window(w1_handle.into(), |_, window, cx| {
        w1_view.update(cx, |q, cx| q.choose_save_destination(None, window, cx));
    })
    .unwrap();
    assert!(cx.did_prompt_for_new_path());
    cx.update(|cx| {
        controller.update(cx, |c, cx| c.activate_tab(w1_id, doc_c, cx).unwrap());
    });
    let copy_wrong_for_picker = copy_wrong.clone();
    cx.simulate_new_path_selection(move |_| Some(copy_wrong_for_picker));
    wait_for_ui(cx, w1_handle, &controller, &w1_view, |q, _, _| {
        !q.confirming
    });
    assert!(!copy_wrong.exists());
    let warning_text = cx.update(|cx| {
        controller
            .read(cx)
            .window_message(w1_id)
            .map(|msg| msg.text.clone())
    });
    assert!(
        warning_text
            .as_deref()
            .is_some_and(|t| t.contains("active document changed")),
        "expected active-document-changed warning, got {warning_text:?}"
    );
}

#[gpui_kit::test]
fn external_open_urls_routes_hardlink_alias_to_owning_window(cx: &mut gpui_kit::TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let orig_path = dir.path().join("owned.csv");
    let alias_path = dir.path().join("hardlink_alias.csv");
    let other_path = dir.path().join("other.csv");
    let fresh_path = dir.path().join("fresh.csv");
    std::fs::write(&orig_path, "id,name\n1,Owned\n").unwrap();
    std::fs::hard_link(&orig_path, &alias_path).unwrap();
    std::fs::write(&other_path, "id,name\n2,Other\n").unwrap();
    std::fs::write(&fresh_path, "id,name\n3,Fresh\n").unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
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

    let (w1_handle, w1_view) = open_test_window(cx, &controller);
    let (w2_handle, w2_view) = open_test_window(cx, &controller);
    let w1_id = cx.update(|cx| w1_view.read(cx).window_id);
    let w2_id = cx.update(|cx| w2_view.read(cx).window_id);

    cx.update_window(w1_handle.into(), |_, window, cx| {
        w1_view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![orig_path.clone()],
                OpenOptions::default(),
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    cx.update_window(w2_handle.into(), |_, window, cx| {
        w2_view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![other_path.clone()],
                OpenOptions::default(),
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, w1_handle, &controller, &w1_view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.index_done)
    });
    wait_for_ui(cx, w2_handle, &controller, &w2_view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|i| i.index_done)
    });
    let doc_orig = cx.update(|cx| {
        controller
            .read(cx)
            .scheduler()
            .active_document(w1_id)
            .unwrap()
    });

    // Simulate Finder opening the hard-link alias while Window 2 is the active/preferred window.
    let alias_url = format!("file://{}", alias_path.display());
    let parsed_alias = crate::file_url_to_path(&alias_url).expect("file URL must parse");
    let route = cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.open_external_paths(Some(w2_id), vec![parsed_alias], cx)
        })
    });
    assert_eq!(
        route.expect("external open should return outcome").unwrap(),
        (w1_id, doc_orig),
        "hard-link alias must route back to Window 1's existing tab"
    );
    cx.update(|cx| {
        let sched = controller.read(cx).scheduler();
        assert_eq!(sched.total_tabs(), 2, "must not duplicate hard-linked file");
        assert_eq!(sched.active_document(w1_id), Some(doc_orig));
    });

    // Opening a brand-new file via external open opens it in the active window (Window 2).
    let fresh_outcome = cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.open_external_paths(Some(w2_id), vec![fresh_path.clone()], cx)
        })
    });
    let (opened_win, opened_doc) = fresh_outcome.unwrap().unwrap();
    assert_eq!(opened_win, w2_id);
    cx.update(|cx| {
        let sched = controller.read(cx).scheduler();
        assert_eq!(sched.total_tabs(), 3);
        assert_eq!(sched.active_document(w2_id), Some(opened_doc));
    });

    // While Window 2 has an unapplied cell editor open, preferred external routing skips it
    // in favor of non-editing Window 1, and directly targeting editing Window 2 preserves its
    // active tab instead of swapping the document out from under the open editor.
    let blocked_path = dir.path().join("blocked_while_editing.csv");
    std::fs::write(&blocked_path, "id,name\n4,Blocked\n").unwrap();
    cx.update(|cx| {
        controller.update(cx, |c, cx| c.set_window_editing(w2_id, true, cx));
        assert_eq!(
            controller
                .read(cx)
                .preferred_window_for_external_open(Some(w2_handle.into())),
            Some(w1_id),
            "preferred external target should pick a non-editing window when active window is editing"
        );
        let blocked = controller.update(cx, |c, cx| {
            c.open_external_paths(Some(w2_id), vec![blocked_path.clone()], cx)
        });
        assert!(
            blocked.expect("outcome").is_err(),
            "opening a new file directly into an editing window must be rejected"
        );
        let sched = controller.read(cx).scheduler();
        assert_eq!(sched.total_tabs(), 3);
        assert_eq!(
            sched.active_document(w2_id),
            Some(opened_doc),
            "editing window must keep its active document untouched"
        );
        // Existing hard-link alias owned by non-editing Window 1 still routes cleanly.
        let alias_routed = controller.update(cx, |c, cx| {
            c.open_external_paths(Some(w2_id), vec![alias_path.clone()], cx)
        });
        assert_eq!(alias_routed.unwrap().unwrap(), (w1_id, doc_orig));
        controller.update(cx, |c, cx| c.set_window_editing(w2_id, false, cx));
    });

    // Opening a modal operation dialog (Sort Rows) in Window 2 also registers it as editing so
    // Finder external opens skip Window 2 and never switch the document underneath the dialog.
    cx.update_window(w2_handle.into(), |_, window, cx| {
        w2_view.update(cx, |q, cx| {
            q.sort_rows_action(&crate::menus::SortRows, window, cx)
        });
    })
    .unwrap();
    cx.update(|cx| {
        assert_eq!(
            controller
                .read(cx)
                .preferred_window_for_external_open(Some(w2_handle.into())),
            Some(w1_id),
            "preferred external target must skip a window with an open Sort dialog"
        );
        let blocked = controller.update(cx, |c, cx| {
            c.open_external_paths(Some(w2_id), vec![blocked_path.clone()], cx)
        });
        assert!(blocked.expect("outcome").is_err());
        assert_eq!(
            controller.read(cx).scheduler().active_document(w2_id),
            Some(opened_doc)
        );
    });
    cx.update_window(w2_handle.into(), |_, window, cx| {
        window.dispatch_action(Box::new(gpui_kit::component::dialog::Cancel), cx);
    })
    .unwrap();
    wait_for_ui(cx, w2_handle, &controller, &w2_view, |q, _, _| !q.editing);
    cx.update(|cx| {
        assert_eq!(
            controller
                .read(cx)
                .preferred_window_for_external_open(Some(w2_handle.into())),
            Some(w2_id),
            "closing the Sort dialog must restore Window 2 as eligible for external opens"
        );
    });

    // If Window 1 owns inactive tab `doc_orig` while editing a second tab, an external open
    // of `alias_path` from preferred Window 2 reports the warning on Window 1 without stealing
    // the warning/activation back onto Window 2.
    let w1_second = dir.path().join("w1_second.csv");
    std::fs::write(&w1_second, "id,name\n5,Second\n").unwrap();
    cx.update_window(w1_handle.into(), |_, window, cx| {
        w1_view.update(cx, |q, cx| {
            q.open_paths_direct(vec![w1_second], OpenOptions::default(), None, window, cx);
        });
    })
    .unwrap();
    wait_for_ui(cx, w1_handle, &controller, &w1_view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| i.document_id != doc_orig && i.index_done)
    });
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.set_window_message(w2_id, None, cx);
            c.set_window_editing(w1_id, true, cx);
        });
        let conflict = controller.update(cx, |c, cx| {
            c.open_external_paths(Some(w2_id), vec![alias_path.clone()], cx)
        });
        assert!(conflict.expect("outcome").is_err());
        assert!(
            controller
                .read(cx)
                .window_message(w1_id)
                .is_some_and(|m| m.text.contains("owning window")),
            "owner-editing rejection warning must be surfaced on editing owner Window 1"
        );
        assert!(
            controller.read(cx).window_message(w2_id).is_none(),
            "non-owning preferred Window 2 must not receive the owner-editing warning"
        );
        controller.update(cx, |c, cx| c.set_window_editing(w1_id, false, cx));
    });
}

#[gpui_kit::test]
fn pause_resume_indexing_and_live_resource_hud_work_through_gpui_ui(cx: &mut TestAppContext) {
    use std::io::Write as _;

    let dir = tempfile::tempdir().unwrap();
    let big_csv = dir.path().join("hud-pausable.csv");
    const TOTAL_ROWS: u64 = 30_000;
    {
        let mut file = std::io::BufWriter::new(std::fs::File::create(&big_csv).unwrap());
        writeln!(file, "id,tag,payload").unwrap();
        let pad = "p".repeat(80);
        for i in 1..=TOTAL_ROWS {
            writeln!(file, "{i},row_{i},{pad}").unwrap();
        }
        file.flush().unwrap();
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
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    // 1. Live Resource HUD samples OS-labelled memory + disk space off-thread.
    wait_for_ui(cx, handle, &controller, &view, |_, _, cx| {
        !controller.read(cx).resource_hud_sample().volumes.is_empty()
    });
    let (hud_mem, hud_vols) = cx.update(|cx| controller.read(cx).resource_hud_for_window(win_id));
    #[cfg(target_os = "macos")]
    assert_eq!(
        hud_mem.map(|m| m.label),
        Some("phys_footprint"),
        "macOS HUD must label memory as phys_footprint"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        hud_mem.map(|m| m.label),
        Some("VmRSS"),
        "Linux HUD must label memory as VmRSS"
    );
    assert!(
        !hud_vols.is_empty(),
        "HUD should report at least one volume's available space"
    );
    let formatted_hud = crate::format_resource_hud(hud_mem, &hud_vols);
    assert!(
        formatted_hud.contains("free"),
        "formatted HUD readout should include free disk space: {formatted_hud:?}"
    );

    // 2. Open CSV with a deterministic test barrier, wait for chunk 0 to commit, then pause.
    let gate = quarry_core::test_hooks::TestIndexPauseGate::install(&big_csv);
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![big_csv.clone()],
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
    assert!(
        gate.wait_until_reached(Duration::from_secs(10)),
        "timed out waiting for UI pause gate to be reached: snapshot={:?}",
        cx.update(|cx| view.read(cx).snapshot(cx)),
    );

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.pause_resume_indexing_action(&crate::menus::PauseResumeIndexing, window, cx);
        });
    })
    .unwrap();
    gate.release();

    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.index_paused || s.index_done)
    });
    let paused_snap = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert!(
        paused_snap.index_paused,
        "expected gated CSV indexing to pause cleanly before EOF"
    );
    assert!(paused_snap.can_resume_indexing);
    assert!(cx.update(|cx| {
        view.read(cx)
            .command_enabled(quarry_workbench::CommandId::PauseResumeIndexing, cx)
    }));

    // Verify status bar shows interactive Resume button and state-transition announcement says paused.
    let paused_announcement = cx.update(|cx| controller.read(cx).hud_announcement(win_id).cloned());
    assert!(
        paused_announcement
            .as_ref()
            .is_some_and(|a| !a.is_alert && a.text.contains("Indexing paused")),
        "expected pause transition announcement, got: {paused_announcement:?}"
    );

    // 3. Click the inline status bar Resume button and verify completion announcement.
    drop(gate);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("index-pause-resume").visible());
        window.click("index-pause-resume", cx);
    })
    .unwrap();

    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });
    let done_snap = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert_eq!(done_snap.total_rows, TOTAL_ROWS);
    assert!(!done_snap.index_paused);
    assert!(!cx.update(|cx| {
        view.read(cx)
            .command_enabled(quarry_workbench::CommandId::PauseResumeIndexing, cx)
    }));
    let done_announcement = cx.update(|cx| controller.read(cx).hud_announcement(win_id).cloned());
    assert!(
        done_announcement
            .as_ref()
            .is_some_and(|a| !a.is_alert && a.text.contains("Indexing complete")),
        "expected completion transition announcement, got: {done_announcement:?}"
    );
}

#[gpui_kit::test]
fn open_section_modal_verify_boundary_and_save_section_as_new_in_gpui(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("section_source.csv");
    // Write > 17 MiB so opening a section past 16 MiB starts as Speculative without a parent index.
    let mut bytes = Vec::from(&b"id,payload\n"[..]);
    let pad = "x".repeat(60);
    let mut row_idx = 0u64;
    while bytes.len() < 17 * 1024 * 1024 {
        bytes.extend_from_slice(format!("{row_idx},{pad}\n").as_bytes());
        row_idx += 1;
    }
    let section_offset = bytes.len() as u64;
    for j in 0..40u64 {
        bytes.extend_from_slice(format!("{},sec_row_{j}\n", row_idx + j).as_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();
    let export_path = dir.path().join("section_saved.csv");

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
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    // 1. Open the Open Section modal dialog from the welcome button and verify it renders cleanly.
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("welcome-open-section").visible());
        window.click("welcome-open-section", cx);
        window.render_frame(cx);
        assert!(window.find("section-browse-file").visible());
        assert!(window.find("section-unit-select").visible());
        assert!(window.find("section-encoding-select").visible());
        assert!(window.find("section-delimiter-select").visible());
        assert!(window.find("section-header-select").visible());
        assert!(window.find("section-quote-free").visible());
        assert!(window.find("section-open-confirm").visible());
        window.click("section-cancel", cx);
        window.render_frame(cx);
    })
    .unwrap();

    // 2. Open a speculative section near 17 MiB via WorkbenchController.
    let window_req = quarry_core::ByteWindow::new(section_offset + 5, 512).unwrap();
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_section(
                    win_id,
                    path.clone(),
                    window_req,
                    quarry_core::SectionOpenOptions::default(),
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.section.is_some() && s.index_done)
    });

    // Verify Speculative badge, Verify Boundary button, in-place Save disabled, Save As enabled.
    let spec_snap = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    let spec_sec = spec_snap.section.as_deref().unwrap();
    assert_eq!(
        spec_sec.bounds.trust,
        quarry_core::BoundaryTrust::Speculative
    );
    assert_eq!(spec_sec.bounds.first_global_row(), None);
    assert!(spec_sec.can_verify_boundary);
    assert!(spec_sec.speculative_save_warning.is_some());
    assert!(!cx.update(|cx| {
        view.read(cx)
            .command_enabled(quarry_workbench::CommandId::Save, cx)
    }));
    assert!(cx.update(|cx| {
        view.read(cx)
            .command_enabled(quarry_workbench::CommandId::SaveAs, cx)
    }));

    // 3. Click the inline Verify Boundary button and wait for Verified promotion with exact global row!
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("section-verify-boundary").visible());
        window.click("section-verify-boundary", cx);
    })
    .unwrap();

    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| {
            s.section.as_deref().is_some_and(|sec| {
                !sec.verify_running && sec.bounds.trust == quarry_core::BoundaryTrust::Verified
            })
        })
    });
    let verified_snap = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    let verified_sec = verified_snap.section.as_deref().unwrap();
    assert_eq!(verified_sec.bounds.first_global_row(), Some(row_idx + 1));
    assert_eq!(verified_sec.first_global_data_row, Some(row_idx + 1));
    assert!(verified_sec.speculative_save_warning.is_none());

    // 4. Save Section As New File and verify byte-0 header + section rows were written cleanly.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_save_active(win_id, None, Some(export_path.clone()), None, cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| !s.saving) && export_path.exists()
    });
    let saved = std::fs::read_to_string(&export_path).unwrap();
    assert!(saved.starts_with("id,payload\n"));
    assert!(saved.contains("sec_row_0"));

    // 5. Open a second section directly through the modal's Open Section submit button (pre-filled
    // from the active tab's path) and confirm the modal submits cleanly and clears editing state.
    let tabs_before = cx.update(|cx| controller.read(cx).tabs_for_window(win_id).len());
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| q.open_section_dialog_now(window, cx));
        window.render_frame(cx);
        assert!(window.find("section-open-confirm").visible());
        window.click("section-open-confirm", cx);
        window.render_frame(cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        controller.read(cx).tabs_for_window(win_id).len() == tabs_before + 1
            && q.snapshot(cx)
                .is_some_and(|s| s.section.is_some() && s.index_done)
    });

    // 6. Edit a data cell in the active section (header rename is disabled on sections),
    // request tab close, and complete Save-and-Close via Save As.
    let close_save_path = dir.path().join("section_close_save_as.csv");
    let dirty_sec_id = cx.update(|cx| view.read(cx).snapshot(cx).unwrap().document_id);
    cx.update(|cx| {
        assert!(
            controller
                .update(cx, |c, cx| {
                    c.rename_header_active(win_id, dirty_sec_id, 0, "edited_id".into(), cx)
                })
                .is_err(),
            "header rename must be disabled on section documents"
        );
        controller
            .update(cx, |c, cx| {
                c.apply_cell_edit(win_id, dirty_sec_id, 1, 1, "EDITED_CELL_VAL".into(), cx)
            })
            .unwrap();
    });
    let review_token = cx.update(|cx| {
        match controller.update(cx, |c, cx| {
            c.request_close_tab(win_id, dirty_sec_id, false, cx)
        }) {
            CloseReviewAction::ConfirmTab {
                review_token,
                document_id,
                ..
            } => {
                assert_eq!(document_id, dirty_sec_id);
                assert!(controller.read(cx).document_is_section(document_id));
                review_token
            }
            other => panic!("expected ConfirmTab for dirty section, got {other:?}"),
        }
    });
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.save_as_in_review(review_token, dirty_sec_id, close_save_path.clone(), cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |_, _, cx| {
        close_save_path.exists()
            && controller
                .read(cx)
                .tabs_for_window(win_id)
                .iter()
                .all(|tab| tab.id != dirty_sec_id)
    });
    let close_saved = std::fs::read_to_string(&close_save_path).unwrap();
    assert!(close_saved.starts_with("id,payload\n"));
    assert!(close_saved.contains("EDITED_CELL_VAL"));
}

#[gpui_kit::test]
fn open_section_dialog_preserves_active_explicit_encoding_for_bomless_utf16le(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bomless_utf16le.csv");
    let text = "id,note\n1,\"hello, world\"\n2,\"second, row\"\n";
    let mut bytes = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();

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
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    // Open the BOM-less UTF-16LE file with an explicit encoding override.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_paths(
                    win_id,
                    vec![path.clone()],
                    quarry_core::OpenOptions::default(),
                    Some(quarry_core::Encoding::Utf16Le),
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });
    let parent_snap = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert_eq!(parent_snap.encoding, quarry_core::Encoding::Utf16Le);
    assert_eq!(
        parent_snap.applied_encoding,
        Some(quarry_core::Encoding::Utf16Le)
    );

    // Open Section… from the active BOM-less UTF-16LE tab and submit without manually re-picking.
    let tabs_before = cx.update(|cx| controller.read(cx).tabs_for_window(win_id).len());
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| q.open_section_dialog_now(window, cx));
        window.render_frame(cx);
        assert!(window.find("section-encoding-select").visible());
        assert!(window.find("section-open-confirm").visible());
        window.click("section-open-confirm", cx);
        window.render_frame(cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        controller.read(cx).tabs_for_window(win_id).len() == tabs_before + 1
            && q.snapshot(cx)
                .is_some_and(|s| s.section.is_some() && s.index_done)
    });

    let sec_snap = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert!(sec_snap.section.is_some());
    assert_eq!(sec_snap.encoding, quarry_core::Encoding::Utf16Le);
    assert_eq!(
        sec_snap.applied_encoding,
        Some(quarry_core::Encoding::Utf16Le)
    );
    assert_eq!(sec_snap.headers, vec!["id".to_string(), "note".to_string()]);
    assert_eq!(sec_snap.total_rows, 2);

    // Also verify explicit NoHeader + delimiter override are preserved when launching
    // Open Section… from an active tab (so a headerless file never becomes headered).
    let no_header_path = dir.path().join("explicit_no_header.csv");
    std::fs::write(&no_header_path, "name;value\nalice;1\nbob;2\n").unwrap();
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_paths(
                    win_id,
                    vec![no_header_path.clone()],
                    quarry_core::OpenOptions {
                        delimiter: Some(b';'),
                        header_mode: quarry_core::HeaderMode::NoHeader,
                        ..quarry_core::OpenOptions::default()
                    },
                    None,
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.path == no_header_path && s.index_done)
    });
    let tabs_before_nh = cx.update(|cx| controller.read(cx).tabs_for_window(win_id).len());
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| q.open_section_dialog_now(window, cx));
        window.render_frame(cx);
        assert!(window.find("section-delimiter-select").visible());
        assert!(window.find("section-header-select").visible());
        window.click("section-open-confirm", cx);
        window.render_frame(cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        controller.read(cx).tabs_for_window(win_id).len() == tabs_before_nh + 1
            && q.snapshot(cx)
                .is_some_and(|s| s.section.is_some() && s.index_done)
    });
    let nh_sec = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert!(nh_sec.section.is_some());
    assert!(!nh_sec.has_header);
    assert_eq!(nh_sec.delimiter, b';');
    assert_eq!(nh_sec.applied_delimiter, Some(b';'));
    assert_eq!(
        nh_sec.applied_header_mode,
        quarry_core::HeaderMode::NoHeader
    );
    assert_eq!(nh_sec.total_rows, 3);
}

#[gpui_kit::test]
fn verified_section_edits_and_splice_save_invalidates_parent_and_blocks_sibling_dirty_edits(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("splice_ui.csv");
    let prefix = "id,tag,note\n0,old,head\n";
    let middle = "1,old,one\n2,keep,two\n3,old,three\n";
    let suffix = "4,old,tail\n";
    std::fs::write(&path, format!("{prefix}{middle}{suffix}")).unwrap();

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
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    // 1. Open parent file and wait for index.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_paths(win_id, vec![path.clone()], Default::default(), None, cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });
    let parent_id = cx.update(|cx| view.read(cx).snapshot(cx).unwrap().document_id);

    // 2. Open verified interior section covering rows 1..=3.
    let sec_start = prefix.len() as u64;
    let sec_len = middle.len() as u64;
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_section(
                    win_id,
                    path.clone(),
                    quarry_core::ByteWindow::new(sec_start, sec_len).unwrap(),
                    quarry_core::SectionOpenOptions::default(),
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.section.is_some() && s.index_done)
    });
    let sec_snap = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    let sec_id = sec_snap.document_id;
    let sec_meta = sec_snap.section.as_deref().unwrap();
    assert_eq!(sec_meta.bounds.trust, quarry_core::BoundaryTrust::Verified);
    assert!(sec_meta.can_splice_save);
    assert!(cx.update(|cx| controller.read(cx).document_can_splice_save(sec_id)));

    // 3. Edit cell (0, 2) -> "ONE_EDITED" on the Verified section.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.apply_cell_edit(win_id, sec_id, 0, 2, "ONE_EDITED".into(), cx)
            })
            .unwrap();
    });

    // 4. Switch to parent tab and confirm dirty mutual exclusion rejects editing parent while
    // section is dirty!
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.activate_tab(win_id, parent_id, cx))
            .unwrap();
    });
    let parent_edit_err = cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.apply_cell_edit(win_id, parent_id, 1, 2, "CONFLICT".into(), cx)
            })
            .unwrap_err()
    });
    assert!(
        parent_edit_err.to_lowercase().contains("unsaved changes"),
        "expected dirty mutual exclusion error, got: {parent_edit_err}"
    );

    // 5. Switch back to section, run Replace All "old" -> "NEW", then Save in-place!
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.activate_tab(win_id, sec_id, cx))
            .unwrap();
        controller
            .update(cx, |c, cx| {
                c.replace_all_active(
                    win_id,
                    sec_id,
                    b"old".to_vec(),
                    b"NEW".to_vec(),
                    quarry_core::CaseSensitivity::Sensitive,
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.document_id == sec_id && !s.structural_running && s.index_done)
    });

    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_save_active(win_id, Some(sec_id), None, None, cx)
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.document_id == sec_id && !s.saving && !s.dirty && s.index_done)
    });

    // Verify parent file bytes: untouched prefix & suffix, updated middle section!
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "id,tag,note\n0,old,head\n1,NEW,ONE_EDITED\n2,keep,two\n3,NEW,three\n4,old,tail\n"
    );

    // Verify section reloaded Verified and parent tab is immediately marked source_changed!
    let after_sec = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert_eq!(
        after_sec.section.as_deref().unwrap().bounds.trust,
        quarry_core::BoundaryTrust::Verified
    );
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.activate_tab(win_id, parent_id, cx))
            .unwrap();
    });
    let parent_snap = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert!(
        parent_snap.source_changed,
        "parent tab must immediately reflect source_changed after section splice-save"
    );
}

#[gpui_kit::test]
fn sort_dialog_confirm_and_cancel_work_after_changing_select_mode(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("numeric_sort.csv");
    std::fs::write(&source, "id,note\n10,ten\n2,two\n100,hundred\n1,one\n").unwrap();
    let controller = init_test_app(cx);
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

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
        quarry
            .snapshot(cx)
            .is_some_and(|i| i.index_done && i.total_rows == 4)
    });

    // Open Sort Rows, change mode Select from Text -> Number (leaving focus on the Select),
    // and click Sort once: the dialog must close and sort numerically (1, 2, 10, 100).
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("toolbar-sort", cx);
        window.render_frame(cx);
        assert!(window.find("apply-sort").visible());
        window.click(("sort-mode", 1usize), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("down", cx);
        window.render_frame(cx);
        window.press("enter", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.focused(cx).is_some(),
            "Select keeps focus after choosing Number"
        );
        window.click("apply-sort", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            !gpui_kit::component::WindowExt::has_active_dialog(window, cx),
            "clicking Sort while a Select holds focus must submit and close the dialog on the first click"
        );
    })
    .unwrap();

    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|i| !i.structural_running && i.index_done)
            && controller
                .read(cx)
                .active_document(win_id)
                .and_then(|d| d.effective_cell(1, 0))
                == Some(b"1".as_slice())
    });

    cx.update(|cx| {
        let doc = controller.read(cx).active_document(win_id).unwrap();
        let ordered: Vec<String> = (1..=4)
            .map(|r| {
                String::from_utf8_lossy(doc.effective_cell(r, 0).unwrap_or_default()).into_owned()
            })
            .collect();
        assert_eq!(
            ordered,
            vec!["1", "2", "10", "100"],
            "Sort by Number must produce numeric order rather than lexicographic order"
        );
    });

    // Re-open Sort Rows, focus a Select again, and verify Cancel closes on the first click.
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("toolbar-sort", cx);
        window.render_frame(cx);
        window.click(("sort-mode", 1usize), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("down", cx);
        window.render_frame(cx);
        window.press("enter", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("cancel-sort", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            !gpui_kit::component::WindowExt::has_active_dialog(window, cx),
            "clicking Cancel while a Select holds focus must close the dialog on the first click"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn active_table_and_list_overlays_stay_translucent_in_light_and_dark_themes(
    cx: &mut TestAppContext,
) {
    let _controller = init_test_app(cx);
    cx.update(|cx| {
        for (preference, appearance, palette) in [
            (
                quarry_workbench::ThemePreference::Light,
                quarry_workbench::ThemeAppearance::Light,
                quarry_workbench::LIGHT,
            ),
            (
                quarry_workbench::ThemePreference::Dark,
                quarry_workbench::ThemeAppearance::Dark,
                quarry_workbench::DARK,
            ),
        ] {
            let resolved = quarry_workbench::ResolvedTheme {
                preference,
                appearance,
                palette,
                source_label: "Built-in".into(),
                warning: None,
            };
            super::apply_resolved_theme(&resolved, cx);
            let theme = gpui_kit::component::Theme::global(cx);
            assert!(
                theme.colors.table_active.a > 0.0 && theme.colors.table_active.a <= 0.30,
                "table_active overlay must stay translucent for {appearance:?} so selected cell text remains readable (got alpha={})",
                theme.colors.table_active.a
            );
            assert!(
                theme.colors.list_active.a > 0.0 && theme.colors.list_active.a <= 0.30,
                "list_active overlay must stay translucent for {appearance:?} so selected list text remains readable (got alpha={})",
                theme.colors.list_active.a
            );
        }
    });
}

#[gpui_kit::test]
fn row_bookmarks_toggle_navigate_sort_undo_and_accessible_labels_in_gpui(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bookmarks_ui.csv");
    std::fs::write(
        &path,
        "id,name\n3,charlie\n1,alpha\n4,delta\n2,bravo\n5,echo\n",
    )
    .unwrap();

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
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path.clone()],
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
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });

    // 1. Toggle bookmarks on physical rows 2 (1,alpha) and 4 (2,bravo); verify snapshot & AX label!
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.toggle_bookmark_row_active(win_id, 2, cx))
            .unwrap();
        controller
            .update(cx, |c, cx| c.toggle_bookmark_row_active(win_id, 4, cx))
            .unwrap();
    });
    let snap = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert_eq!(snap.bookmark_count, 2);
    assert_eq!(snap.bookmarks.len(), 2);
    assert_eq!(snap.bookmarks[0].local_row, 2);
    assert_eq!(snap.bookmarks[1].local_row, 4);
    assert!(snap.bookmarks[0].record_start_offset.is_some());
    assert_eq!(snap.split_at_bookmarks_disabled_reason, None);
    assert!(!snap.dirty, "bookmarks must not mark document dirty");

    // Verify accessible row gutter label includes ", bookmarked" for row 2 (table row 1) and not row 1 (table row 0).
    let (row0_label, row1_label) = cx.update(|cx| {
        let table = view.read(cx).table.read(cx);
        let delegate = table.delegate();
        use gpui_kit::component::table::TableDelegate;
        (
            delegate.cell_label(0, 0, cx).map(|s| s.to_string()),
            delegate.cell_label(1, 0, cx).map(|s| s.to_string()),
        )
    });
    assert_eq!(row0_label.as_deref(), Some("Row 1"));
    assert_eq!(row1_label.as_deref(), Some("Row 2, bookmarked"));

    // 2. Step Next / Previous Bookmark wraps cleanly and selects target row.
    let (next_table_row, _) = cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.step_bookmark_active(win_id, true, cx))
            .unwrap()
    });
    assert_eq!(next_table_row, 1); // physical row 2 -> table row 1
    let (next2_table_row, _) = cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.step_bookmark_active(win_id, true, cx))
            .unwrap()
    });
    assert_eq!(next2_table_row, 3); // physical row 4 -> table row 3
    let (prev_table_row, _) = cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.step_bookmark_active(win_id, false, cx))
            .unwrap()
    });
    assert_eq!(prev_table_row, 1);

    // 3. Sort clears bookmarks with notice; Undo restores them!
    let doc_id = snap.document_id;
    cx.update(|cx| {
        let plan = quarry_core::SortPlan::from(quarry_core::SortSpec {
            column: 0,
            mode: quarry_core::SortMode::Number,
            direction: quarry_core::SortDirection::Ascending,
            case_sensitivity: quarry_core::CaseSensitivity::Insensitive,
        });
        controller
            .update(cx, |c, cx| c.start_sort_active(win_id, doc_id, plan, cx))
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| !s.structural_running && s.index_done && s.dirty)
    });
    let after_sort = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert_eq!(after_sort.bookmark_count, 0);

    // Undo Sort restores both bookmarks!
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.undo_action(&crate::menus::UndoEdit, window, cx);
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.index_done && s.bookmark_count == 2)
    });

    // 4. Clear bookmarks removes all bookmarks cleanly.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.clear_bookmarks_action(&crate::menus::ClearBookmarks, window, cx);
        });
    })
    .unwrap();
    let after_clear = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert_eq!(after_clear.bookmark_count, 0);
}

#[gpui_kit::test]
fn row_bookmarks_filtered_stepping_advances_and_wraps_cleanly(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bookmarks_filtered_ui.csv");
    std::fs::write(
        &path,
        "id,name,group\n1,alpha,a\n2,beta,b\n3,gamma,a\n4,delta,b\n5,epsilon,a\n6,zeta,b\n",
    )
    .unwrap();

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
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path.clone()],
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
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });

    // Bookmark all 3 group-a rows (physical rows 1, 3, 5) and activate a filter for group == a.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.toggle_bookmark_row_active(win_id, 1, cx).unwrap();
                c.toggle_bookmark_row_active(win_id, 3, cx).unwrap();
                c.toggle_bookmark_row_active(win_id, 5, cx).unwrap();
                c.start_filter_active(
                    win_id,
                    quarry_core::FilterQuery::single(
                        2,
                        quarry_core::FilterOperator::Equals,
                        b"a".to_vec(),
                    ),
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| {
            s.filter_active && s.filter_done && !s.filter_running && s.available_rows == 3
        })
    });

    // Repeated Next Bookmark must advance through filtered rows (0 -> 1 -> 2 -> 0) without stalling!
    let (r1, _) = cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.step_bookmark_active(win_id, true, cx))
            .unwrap()
    });
    let (r2, _) = cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.step_bookmark_active(win_id, true, cx))
            .unwrap()
    });
    let (r3, _) = cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.step_bookmark_active(win_id, true, cx))
            .unwrap()
    });
    assert_eq!((r1, r2, r3), (1, 2, 0));

    // Previous Bookmark must wrap cleanly from 0 -> 2 -> 1.
    let (p1, _) = cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.step_bookmark_active(win_id, false, cx))
            .unwrap()
    });
    let (p2, _) = cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.step_bookmark_active(win_id, false, cx))
            .unwrap()
    });
    assert_eq!((p1, p2), (2, 1));
}

#[gpui_kit::test]
fn row_copy_works_after_cell_selection_and_under_active_filter(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("row_copy_parity.csv");
    std::fs::write(&path, "id,name,group\n1,alpha,a\n2,beta,b\n3,gamma,a\n").unwrap();

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
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path.clone()],
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
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });

    // Select a cell first, then select a row via gutter click; Cmd+C must copy the full row TSV.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.table.update(cx, |t, cx| t.set_selected_cell(0, 1, cx));
            controller.update(cx, |c, cx| {
                c.select_cell_active(win_id, Some((1, 0)), cx);
                c.select_row_active(win_id, 2, false, false, cx);
            });
            q.copy_action(&crate::CopySelection, window, cx);
        });
    })
    .unwrap();

    let copied = cx.update(|cx| {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default()
    });
    assert_eq!(copied, "2\tbeta\tb");

    // Apply a filter and confirm row gutter selection & row copy still work while filtered.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_filter_active(
                    win_id,
                    quarry_core::FilterQuery::single(
                        2,
                        quarry_core::FilterOperator::Equals,
                        b"a".to_vec(),
                    ),
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.filter_active && s.filter_done && s.available_rows == 2)
    });

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            controller.update(cx, |c, cx| {
                c.select_row_active(win_id, 3, false, false, cx);
            });
            q.open_context_menu_action(&crate::OpenContextMenu, window, cx);
            assert!(q.grid_context_menu.is_some());
            assert_eq!(
                q.table.read(cx).selected_cell(),
                Some((1, 0)),
                "Shift+F10 on filtered source row 3 (match index 1) must select table row 1, not source offset 2"
            );
            q.close_grid_context_menu(cx);
            q.copy_action(&crate::CopySelection, window, cx);
        });
    })
    .unwrap();
    let copied_filtered = cx.update(|cx| {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default()
    });
    assert_eq!(copied_filtered, "3\tgamma\ta");

    // Keyboard navigation within column 0 (row gutter) must synchronize document row selection
    // so Cmd/Ctrl+C copies the newly navigated row rather than the old clicked row.
    cx.update_window(handle.into(), |_, _, cx| {
        view.update(cx, |q, cx| {
            q.table.update(cx, |t, cx| t.set_selected_cell(0, 0, cx));
        });
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.copy_action(&crate::CopySelection, window, cx);
        });
    })
    .unwrap();
    let copied_navigated = cx.update(|cx| {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default()
    });
    assert_eq!(copied_navigated, "1\talpha\ta");

    // Shift-selecting across filtered rows (source rows 1 and 3 match; row 2 does not) must
    // select only the 2 matching rows rather than physical range 1..=3 (count 3).
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.select_row_active(win_id, 3, true, false, cx);
        });
    });
    let filtered_shift_count = cx.update(|cx| {
        controller
            .read(cx)
            .active_snapshot(win_id)
            .expect("active snapshot")
            .selected_rows_count
    });
    assert_eq!(
        filtered_shift_count, 2,
        "Shift-selection under a filter must select only matching rows, not hidden rows between them"
    );
}

#[gpui_kit::test]
fn filtered_bookmark_jump_updates_filtered_row_selection_anchor(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("filtered_bookmark_anchor.csv");
    std::fs::write(
        &path,
        "id,name,tag\n1,row1,a\n2,row2,b\n3,row3,a\n4,row4,b\n5,row5,a\n",
    )
    .unwrap();

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
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path.clone()],
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
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });

    // Bookmark source row 3 before filtering to tag == "a" (matches source rows 1, 3, 5).
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.select_row_active(win_id, 3, false, false, cx);
            c.toggle_bookmark_active(win_id, cx).unwrap();
            c.filter_by_cell_value_active(
                win_id,
                2,
                b"a".to_vec(),
                true,
                quarry_core::CaseSensitivity::Sensitive,
                cx,
            )
            .unwrap();
        });
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.filter_active && s.filter_done && s.available_rows == 3)
    });

    // Select source row 1 (filtered ordinal 0), jump to bookmarked source row 3 (filtered ordinal 1),
    // then Shift-click source row 5 (filtered ordinal 2). Only rows 3 and 5 must be selected.
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.select_row_active(win_id, 1, false, false, cx);
            let (table_row, _) = c.jump_to_bookmark_active(win_id, 3, cx).unwrap();
            assert_eq!(table_row, 1);
            c.select_row_active(win_id, 5, true, false, cx);
        });
    });

    cx.update(|cx| {
        let controller_ref = controller.read(cx);
        let snap = controller_ref
            .active_snapshot(win_id)
            .expect("active snapshot");
        assert_eq!(
            snap.selected_rows_count, 2,
            "Shift-clicking row 5 after filtered bookmark jump to row 3 must select only 2 rows"
        );
        let doc = controller_ref
            .active_document(win_id)
            .expect("active document");
        assert!(
            !doc.is_table_row_selected(0, 1),
            "source row 1 (ordinal 0) must not remain selected"
        );
        assert!(
            doc.is_table_row_selected(1, 3),
            "bookmarked source row 3 (ordinal 1) must be selected"
        );
        assert!(
            doc.is_table_row_selected(2, 5),
            "Shift-clicked source row 5 (ordinal 2) must be selected"
        );
    });
}

#[gpui_kit::test]
fn rename_header_targets_selected_column_and_context_menus_work(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("context_menu_parity.csv");
    std::fs::write(
        &path,
        "id,name,city\n1,Alice,Boston\n2,Bob,Denver\n3,Cara,Boston\n",
    )
    .unwrap();

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
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.open_paths_direct(
                vec![path.clone()],
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
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });

    // Cell is at column 0 (`id`), but user selects column 2 (`city`). Rename must target column 2.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.table.update(cx, |t, cx| t.set_selected_cell(0, 1, cx));
            controller.update(cx, |c, cx| {
                c.select_column_active(win_id, 2, false, false, cx);
            });
            q.open_context_menu_action(&crate::OpenContextMenu, window, cx);
            assert!(
                q.grid_context_menu.is_some(),
                "Shift+F10 on selected column must open column context menu"
            );
            q.close_grid_context_menu(cx);

            // Invoke Rename Selected Column Header… with no explicit column: must target selected
            // column 2 (`city`) rather than focused cell column 0 (`id`).
            q.show_header_rename_dialog(None, window, cx);
            assert!(q.editing, "header rename dialog must open");
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, window, _| {
        q.editing && window.try_find("apply-header-rename").is_some()
    });
    cx.update_window(handle.into(), |_, window, cx| {
        let input = snapshots(window)
            .into_iter()
            .find(|element| element.label() == Some("Column header name"))
            .expect("the dialog must render its accessible header input");
        assert_eq!(
            input.value(),
            Some("city"),
            "rename dialog must pre-fill selected column 2 (`city`), not focused column 0 (`id`)"
        );
        window
            .within("dialog")
            .click(input.path().last().unwrap().clone(), cx);
        window.press(&format!("{PRIMARY_MOD}-a"), cx);
        cx.write_to_clipboard(ClipboardItem::new_string("metro_area".to_string()));
        window.press(&format!("{PRIMARY_MOD}-v"), cx);
        window.click("apply-header-rename", cx);
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, window, _| {
        !q.editing && window.try_find("apply-header-rename").is_none()
    });
    let headers_after_rename = cx.update(|cx| {
        controller
            .read(cx)
            .active_snapshot(win_id)
            .expect("active snapshot")
            .headers
    });
    assert_eq!(
        headers_after_rename[0], "id",
        "focused cell column 0 must remain unchanged"
    );
    assert_eq!(
        headers_after_rename[2], "metro_area",
        "selected column 2 must receive the submitted rename"
    );

    // Open row gutter context menu via GridAction and verify row gets auto-selected.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.handle_grid_action(
                crate::GridAction::RowContextMenu {
                    table_row: 1,
                    source_row: 2,
                    position: gpui_kit::Point::new(px(24.), px(120.)),
                },
                window,
                cx,
            );
            assert!(q.grid_context_menu.is_some());
            q.close_grid_context_menu(cx);
        });
    })
    .unwrap();
    let row_sel = cx.update(|cx| controller.read(cx).active_selection(win_id));
    assert_eq!(
        row_sel,
        Some(quarry_workbench::GridSelection::Row { row: 2 })
    );

    // Open cell context menu and verify table selection syncs to the right-clicked cell before
    // invoking quick Filter to This Value (`city == Boston` -> 2 rows).
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.handle_grid_action(
                crate::GridAction::CellContextMenu {
                    table_row: 0,
                    table_col: 3,
                    source_row: 1,
                    source_col: 2,
                    position: gpui_kit::Point::new(px(260.), px(100.)),
                },
                window,
                cx,
            );
            assert!(q.grid_context_menu.is_some());
            assert_eq!(
                q.table.read(cx).selected_cell(),
                Some((0, 3)),
                "right-clicking a cell must sync TableState::selected_cell()"
            );
            q.close_grid_context_menu(cx);
            q.apply_quick_cell_filter(
                2,
                b"Boston".to_vec(),
                true,
                quarry_core::CaseSensitivity::Insensitive,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.filter_active && s.filter_done && s.available_rows == 2)
    });

    // Quick Filter Out This Value (`city != Boston` -> 1 row).
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.apply_quick_cell_filter(
                2,
                b"Boston".to_vec(),
                false,
                quarry_core::CaseSensitivity::Insensitive,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.filter_active && s.filter_done && s.available_rows == 1)
    });
}

#[gpui_kit::test]
fn whitespace_persists_damaged_gzip_uninspectable_and_drag_drop_opens_files(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let pref_path = dir.path().join("view-preferences");
    let valid_csv = dir.path().join("ws_ragged.csv");
    std::fs::write(
        &valid_csv,
        "id,name,extra\n1,alpha,10\n\"  hi\tthere\n\",x\n",
    )
    .unwrap();
    let bad_gz = dir.path().join("broken.csv.gz");
    std::fs::write(&bad_gz, b"corrupt gzip payload").unwrap();
    let subfolder = dir.path().join("folder");
    std::fs::create_dir_all(&subfolder).unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::apply_light_theme(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        let prefs = quarry_workbench::whitespace::Preferences::load(Some(pref_path.clone()));
        cx.new(|_| {
            WorkbenchController::new(WorkbenchSchedulerConfig {
                index_cache_enabled: false,
                ..WorkbenchSchedulerConfig::default()
            })
            .with_whitespace_prefs(prefs)
        })
    });
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    // Toggle whitespace and verify persistence to disk.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.toggle_whitespace_action(&crate::ShowAllWhitespace, window, cx);
        });
    })
    .unwrap();
    assert!(cx.update(|cx| controller.read(cx).show_all_whitespace()));
    assert!(quarry_workbench::whitespace::Preferences::load(Some(pref_path)).show_all);

    // Dropping only a folder warns cleanly without opening a tab.
    cx.update_window(handle.into(), |_, _, cx| {
        view.update(cx, |q, cx| {
            q.handle_dropped_paths(vec![subfolder.clone()], cx);
        });
    })
    .unwrap();
    assert!(cx.update(|cx| view.read(cx).snapshot(cx).is_none()));
    assert!(cx.update(|cx| {
        controller
            .read(cx)
            .window_message(win_id)
            .is_some_and(|m| m.text.contains("folders cannot be opened"))
    }));

    // Dropping a real CSV file opens it and exposes raw whitespace + absent ragged column 2.
    cx.update_window(handle.into(), |_, _, cx| {
        view.update(cx, |q, cx| {
            q.handle_dropped_paths(vec![valid_csv.clone()], cx);
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });

    let (raw_cell_0, absent_cell_2, absent_label) = cx.update(|cx| {
        let table = view.read(cx).table.read(cx);
        let c0 = table.delegate().cell_raw_value(1, 1, cx);
        let c2 = table.delegate().cell_raw_value(1, 3, cx);
        let lbl = table.delegate().cell_label(1, 3, cx);
        (c0, c2, lbl)
    });
    assert_eq!(
        raw_cell_0.and_then(|(_, _, v)| v),
        Some(b"  hi\tthere\n".to_vec())
    );
    assert_eq!(absent_cell_2.and_then(|(_, _, v)| v), None);
    assert_eq!(
        absent_label.as_deref(),
        Some("extra: Field is absent from this row.")
    );

    // Opening a damaged .gz file returns an error WITHOUT an inspection target.
    let gz_err = cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.open_paths(
                win_id,
                vec![bad_gz.clone()],
                OpenOptions::default(),
                None,
                cx,
            )
        })
    });
    let err_msg = gz_err.expect_err("damaged .gz must fail");
    assert!(
        err_msg.inspection_target.is_none(),
        "damaged .gz open failure must not offer Inspect… button"
    );
}

#[gpui_kit::test]
fn adaptive_word_wrap_persists_preserves_anchor_and_auto_disables_above_threshold(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let pref_path = dir.path().join("word-wrap-preferences");
    let small_csv = dir.path().join("small_wrap.csv");
    let large_csv = dir.path().join("large_wrap.csv");

    let mut small_content = String::from("id,notes\n");
    for i in 1..=120 {
        small_content.push_str(&format!(
            "{i},\"Row {i} has a deliberately long note value designed to wrap cleanly onto two visual lines inside a narrow grid column\"\n"
        ));
    }
    std::fs::write(&small_csv, &small_content).unwrap();

    let mut large_content = String::from("id,notes\n");
    for i in 1..=600 {
        large_content.push_str(&format!(
            "{i},\"Large file row {i} payload exceeding a low custom auto-disable threshold for deterministic adaptive word-wrap verification\"\n"
        ));
    }
    std::fs::write(&large_csv, &large_content).unwrap();
    let small_len = std::fs::metadata(&small_csv).unwrap().len();
    let large_len = std::fs::metadata(&large_csv).unwrap().len();
    assert!(small_len < large_len);

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::apply_light_theme(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        let prefs = quarry_workbench::WordWrapPreferences::load(Some(pref_path.clone()));
        cx.new(|_| {
            WorkbenchController::new(WorkbenchSchedulerConfig {
                index_cache_enabled: false,
                ..WorkbenchSchedulerConfig::default()
            })
            .with_word_wrap_prefs(prefs)
        })
    });
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    // Open small CSV and wait for indexing.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_paths(
                    win_id,
                    vec![small_csv.clone()],
                    OpenOptions::default(),
                    None,
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });

    assert_eq!(
        cx.update(|cx| controller.read(cx).effective_word_wrap(win_id)),
        quarry_workbench::EffectiveWordWrap::Off
    );

    // Scroll to row 50, then toggle word wrap ON and confirm anchor + persistence + clean state.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.row_input
                .update(cx, |input, cx| input.set_value("51", window, cx));
            q.go_to_row(cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    let anchor_before =
        cx.update(|cx| view.read(cx).table.read(cx).delegate().visible_rows().start);

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.toggle_word_wrap_action(&crate::menus::ToggleWordWrap, window, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();

    let snap_on = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert_eq!(
        snap_on.word_wrap,
        quarry_workbench::EffectiveWordWrap::Active {
            manual_override: false
        }
    );
    assert!(
        !snap_on.dirty,
        "word wrap must remain view-only and never dirty a document"
    );
    assert!(
        quarry_workbench::WordWrapPreferences::load(Some(pref_path.clone())).enabled,
        "word wrap preference must persist across restarts"
    );
    let anchor_after = cx.update(|cx| view.read(cx).table.read(cx).delegate().visible_rows().start);
    assert_eq!(
        anchor_after, anchor_before,
        "toggling word wrap on must preserve the top visible row anchor"
    );
    let (cache_len, _, _) =
        cx.update(|cx| view.read(cx).table.read(cx).delegate().wrap_cache_stats());
    assert!(
        cache_len > 0 && cache_len <= quarry_workbench::MAX_CACHED_WRAP_ROWS,
        "expected fetched-row wrap cache to populate bounded entries; got {cache_len}"
    );

    // Lower threshold strictly between small_len and large_len to verify deterministic auto-disable.
    let threshold = small_len + 1;
    assert!(large_len > threshold);
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.set_word_wrap_auto_disable_bytes(threshold, cx)
            })
            .unwrap();
    });
    assert_eq!(
        quarry_workbench::WordWrapPreferences::load(Some(pref_path)).auto_disable_bytes,
        threshold
    );

    // Opening large_csv while word wrap is globally enabled must auto-disable for that tab.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_paths(
                    win_id,
                    vec![large_csv.clone()],
                    OpenOptions::default(),
                    None,
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.path == large_csv && s.index_done)
    });

    let snap_large = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert_eq!(
        snap_large.word_wrap,
        quarry_workbench::EffectiveWordWrap::AutoDisabled {
            file_bytes: large_len,
            threshold_bytes: threshold,
        }
    );

    // Toggling on an auto-disabled tab overrides it ON for that tab only; toggling again returns to AutoDisabled
    // and emits quiet transition announcements for assistive technology.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.toggle_word_wrap_action(&crate::menus::ToggleWordWrap, window, cx);
        });
    })
    .unwrap();
    assert_eq!(
        cx.update(|cx| controller.read(cx).effective_word_wrap(win_id)),
        quarry_workbench::EffectiveWordWrap::Active {
            manual_override: true
        }
    );
    let override_announcement = cx.update(|cx| {
        controller
            .read(cx)
            .hud_announcement(win_id)
            .map(|a| a.text.clone())
    });
    assert!(
        override_announcement
            .as_deref()
            .is_some_and(|t| t.contains("Word wrap enabled for this tab")),
        "override toggle must emit a quiet accessibility announcement; got {override_announcement:?}"
    );

    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.toggle_word_wrap_action(&crate::menus::ToggleWordWrap, window, cx);
        });
    })
    .unwrap();
    assert!(cx.update(|cx| {
        controller
            .read(cx)
            .effective_word_wrap(win_id)
            .is_auto_disabled()
    }));
    let auto_off_announcement = cx.update(|cx| {
        controller
            .read(cx)
            .hud_announcement(win_id)
            .map(|a| a.text.clone())
    });
    assert!(
        auto_off_announcement
            .as_deref()
            .is_some_and(|t| t.contains("auto-disabled")),
        "auto-disable transition must emit a quiet accessibility announcement; got {auto_off_announcement:?}"
    );
}

#[gpui_kit::test]
fn adaptive_word_wrap_resets_frame_budget_on_single_row_and_bounds_wide_csvs(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let single_row_csv = dir.path().join("single_row_4k.csv");
    let wide_csv = dir.path().join("wide_500_cols.csv");

    let payload_4k = "word ".repeat(900); // > 4 KiB
    std::fs::write(&single_row_csv, format!("id,notes\n1,\"{payload_4k}\"\n")).unwrap();

    let mut wide_header = String::new();
    let mut wide_row = String::new();
    for col in 0..500 {
        if col > 0 {
            wide_header.push(',');
            wide_row.push(',');
        }
        wide_header.push_str(&format!("c{col}"));
        wide_row.push_str(&format!("Cell {col} wrapped preview text value"));
    }
    let mut wide_body = format!("{wide_header}\n");
    for _ in 0..3 {
        wide_body.push_str(&wide_row);
        wide_body.push('\n');
    }
    std::fs::write(&wide_csv, wide_body).unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::apply_light_theme(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        let mut prefs = quarry_workbench::WordWrapPreferences::load(None);
        prefs.set_enabled(true).unwrap();
        cx.new(|_| {
            WorkbenchController::new(WorkbenchSchedulerConfig {
                index_cache_enabled: false,
                ..WorkbenchSchedulerConfig::default()
            })
            .with_word_wrap_prefs(prefs)
        })
    });
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    // 1. Open single-row 4 KiB CSV and redraw 40 consecutive frames: the wrap frame budget
    // must reset at each frame boundary so the row never degrades after 32 frames.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_paths(
                    win_id,
                    vec![single_row_csv.clone()],
                    OpenOptions::default(),
                    None,
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });

    for _ in 0..40 {
        cx.update_window(handle.into(), |_, _, cx| {
            view.update(cx, |q, cx| {
                q.table.update(cx, |_, cx| cx.notify());
                cx.notify();
            });
        })
        .unwrap();
        cx.update(|cx| cx.refresh_windows());
        cx.run_until_parked();
    }

    let (used_bytes, clamped_cells) = cx.update(|cx| {
        view.read(cx)
            .table
            .read(cx)
            .delegate()
            .wrap_frame_budget_stats()
    });
    assert_eq!(
        clamped_cells, 0,
        "1-row 4 KiB document must reset wrap frame budget across 40 consecutive frames"
    );
    assert!(
        used_bytes > 0 && used_bytes <= quarry_workbench::MAX_WRAP_CELL_BYTES + 64,
        "expected single-frame consumption bounded to visible 1-row cells; got {used_bytes} B"
    );

    // 2. Open a 500-column CSV and confirm wrap measurement only touches horizontally visible
    // viewport cells rather than all 500 unhidden columns per row.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_paths(
                    win_id,
                    vec![wide_csv.clone()],
                    OpenOptions::default(),
                    None,
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.path == wide_csv && s.index_done)
    });
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();

    let (cached_rows, cached_cells, wide_used, _) = cx.update(|cx| {
        let table = view.read(cx).table.read(cx);
        let d = table.delegate();
        let (rows, _, _) = d.wrap_cache_stats();
        let cells = d.wrap_cache_cell_count();
        let (used, clamped) = d.wrap_frame_budget_stats();
        (rows, cells, used, clamped)
    });
    assert!(
        cached_rows >= 1,
        "expected visible rows cached on wide CSV; got {cached_rows}"
    );
    assert!(
        cached_cells > 0 && cached_cells < 150,
        "wide 500-column CSV must cache only horizontally visible viewport cells, not all 1500 offscreen cells; got {cached_cells}"
    );
    assert!(
        wide_used <= quarry_workbench::MAX_WRAP_FRAME_BUDGET_BYTES,
        "wide CSV frame budget must remain <= 128 KiB; got {wide_used}"
    );
}

#[gpui_kit::test]
fn live_watch_streams_appended_rows_pauses_scroll_and_handles_rotation(cx: &mut TestAppContext) {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let csv_path = dir.path().join("live_feed.csv");
    let mut initial = String::from("id,event\n");
    for i in 1..=50 {
        initial.push_str(&format!("{i},seed-{i:03}\n"));
    }
    std::fs::write(&csv_path, &initial).unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::apply_light_theme(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        cx.new(|_| {
            WorkbenchController::new(WorkbenchSchedulerConfig {
                index_cache_enabled: false,
                ..WorkbenchSchedulerConfig::default()
            })
        })
    });
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_paths(
                    win_id,
                    vec![csv_path.clone()],
                    OpenOptions::default(),
                    None,
                    cx,
                )
            })
            .unwrap();
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });

    // Toggle Live Watch ON via the GPUI action.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.toggle_live_watch_action(&crate::menus::ToggleLiveWatch, window, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();

    let snap_on = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert!(snap_on.live_watch.active);
    assert!(snap_on.live_watch.tail_follow);
    assert!(
        cx.update(|cx| controller.read(cx).hud_announcement(win_id).cloned())
            .is_some_and(|a| !a.is_alert && a.text.contains("Live Watch started")),
        "starting Live Watch must emit a quiet transition announcement"
    );

    // Pause tail-follow by jumping up to row 1 via the real navigation/scroll path, then append 20 new rows.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.row_input
                .update(cx, |input, cx| input.set_value("1", window, cx));
            q.go_to_row(cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    let snap_after_jump = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert!(
        !snap_after_jump.live_watch.tail_follow,
        "navigating up to row 1 while watching must pause tail-follow automatically"
    );

    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&csv_path)
        .unwrap();
    for i in 51..=70 {
        writeln!(writer, "{i},stream-{i:03}").unwrap();
    }
    writer.flush().unwrap();

    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| s.total_rows == 70)
    });
    let snap_paused = cx.update(|cx| view.read(cx).snapshot(cx).unwrap());
    assert!(snap_paused.live_watch.active);
    assert!(!snap_paused.live_watch.tail_follow);
    assert_eq!(snap_paused.live_watch.appended_rows, 20);

    // Rename-rotate the file and verify the UI surfaces the Rotated alert and emits Role::Alert.
    std::fs::rename(&csv_path, dir.path().join("live_feed.csv.1")).unwrap();
    std::fs::write(&csv_path, b"id,event\n100,rotated-row\n").unwrap();

    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.live_watch.alert == Some(quarry_core::WatchAlert::Rotated))
    });
    assert!(
        cx.update(|cx| controller.read(cx).hud_announcement(win_id).cloned())
            .is_some_and(|a| a.is_alert && a.text.contains("rotated")),
        "rotation alert must emit a Role::Alert announcement"
    );

    // Clicking "Open New File" (ReloadFromDisk) loads the rotated replacement cleanly.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.reload_action(&crate::menus::ReloadFromDisk, window, cx);
        });
    })
    .unwrap();
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.index_done && s.total_rows == 1 && !s.source_changed)
    });
}

#[gpui_kit::test]
fn live_watch_coalesces_tail_refreshes_while_viewport_read_is_in_flight(cx: &mut TestAppContext) {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let csv_path = dir.path().join("coalesce_feed.csv");
    let mut initial = String::from("id,event\n");
    for i in 1..=50 {
        initial.push_str(&format!("{i},seed-{i:03}\n"));
    }
    std::fs::write(&csv_path, &initial).unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::apply_light_theme(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        cx.new(|_| {
            WorkbenchController::new(WorkbenchSchedulerConfig {
                index_cache_enabled: false,
                ..WorkbenchSchedulerConfig::default()
            })
        })
    });
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    let (_, doc_id) = cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.open_paths(
                    win_id,
                    vec![csv_path.clone()],
                    OpenOptions::default(),
                    None,
                    cx,
                )
            })
            .unwrap()
    });
    wait_for_ui(cx, handle, &controller, &view, |q, _, cx| {
        q.snapshot(cx).is_some_and(|s| s.index_done)
    });

    // Start Live Watch with tail-follow enabled and drain the initial tail read.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.toggle_live_watch_action(&crate::menus::ToggleLiveWatch, window, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!cx.update(|cx| {
        controller
            .read(cx)
            .has_live_watch_tail_read_in_flight(doc_id)
    }));

    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&csv_path)
        .unwrap();

    // Batch 1: append 10 rows and poll until the first tail refresh is queued (still in flight
    // because we do not run the GPUI background executor yet).
    for i in 51..=60 {
        writeln!(writer, "{i},stream-{i:03}").unwrap();
    }
    writer.flush().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let ready = cx.update(|cx| {
            controller.update(cx, |c, cx| {
                c.poll_background(cx);
            });
            controller
                .read(cx)
                .active_document(win_id)
                .is_some_and(|doc| doc.available_rows() >= 61)
                && controller
                    .read(cx)
                    .has_live_watch_tail_read_in_flight(doc_id)
        });
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for first live-watch tail read to be scheduled"
        );
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    let seq_in_flight = cx
        .update(|cx| controller.read(cx).viewport_read_sequence(doc_id))
        .expect("tail read sequence must be recorded");

    // Batch 2: append 10 more rows while batch 1's viewport read is still in flight.
    // Polling must coalesce the dirty refresh rather than incrementing the sequence and
    // superseding the in-flight read.
    for i in 61..=70 {
        writeln!(writer, "{i},stream-{i:03}").unwrap();
    }
    writer.flush().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let ready = cx.update(|cx| {
            controller.update(cx, |c, cx| {
                c.poll_background(cx);
            });
            controller
                .read(cx)
                .active_document(win_id)
                .is_some_and(|doc| doc.available_rows() >= 71)
        });
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for second append batch during in-flight tail read"
        );
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    assert_eq!(
        cx.update(|cx| controller.read(cx).viewport_read_sequence(doc_id)),
        Some(seq_in_flight),
        "arriving appends during an in-flight tail read must coalesce without bumping viewport_read_seq"
    );
    assert!(
        cx.update(|cx| controller
            .read(cx)
            .has_live_watch_tail_read_in_flight(doc_id)),
        "original tail read must remain in flight until completion"
    );

    // Completing the background executor installs batch 1 and immediately dispatches and completes
    // the single coalesced follow-up read for batch 2.
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| controller.read(cx).viewport_read_sequence(doc_id)),
        Some(seq_in_flight.wrapping_add(1)),
        "completing the first tail read must dispatch exactly one coalesced follow-up read"
    );
    assert!(!cx.update(|cx| {
        controller
            .read(cx)
            .has_live_watch_tail_read_in_flight(doc_id)
    }));
    let saw_row_70 = cx.update(|cx| {
        controller
            .read(cx)
            .active_document(win_id)
            .is_some_and(|doc| {
                doc.buffered_rows.iter().any(|row| {
                    row.fields
                        .get(1)
                        .is_some_and(|v| v.as_slice() == b"stream-070")
                })
            })
    });
    assert!(
        saw_row_70,
        "coalesced follow-up tail read must install the latest appended rows into the viewport"
    );

    // Explicit navigation still supersedes an in-flight tail read immediately.
    for i in 71..=75 {
        writeln!(writer, "{i},stream-{i:03}").unwrap();
    }
    writer.flush().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let ready = cx.update(|cx| {
            controller.update(cx, |c, cx| {
                c.poll_background(cx);
            });
            controller
                .read(cx)
                .active_document(win_id)
                .is_some_and(|doc| doc.available_rows() >= 76)
                && controller
                    .read(cx)
                    .has_live_watch_tail_read_in_flight(doc_id)
        });
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for third tail read to enter flight"
        );
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    let seq_before_nav = cx
        .update(|cx| controller.read(cx).viewport_read_sequence(doc_id))
        .unwrap();
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.request_viewport_rows(win_id, 1, 20, cx);
        });
    });
    assert_eq!(
        cx.update(|cx| controller.read(cx).viewport_read_sequence(doc_id)),
        Some(seq_before_nav.wrapping_add(1)),
        "explicit navigation must immediately supersede an in-flight tail read"
    );
    assert!(!cx.update(|cx| {
        controller
            .read(cx)
            .has_live_watch_tail_read_in_flight(doc_id)
    }));
    cx.run_until_parked();
    let nav_row_1 = cx.update(|cx| {
        controller.read(cx).active_document(win_id).and_then(|doc| {
            doc.visible_row(0)
                .and_then(|(_, fields)| fields.get(1))
                .cloned()
        })
    });
    assert_eq!(
        nav_row_1.as_deref(),
        Some(b"seed-002".as_slice()),
        "superseded tail read must not overwrite rows loaded by explicit user navigation"
    );
}

#[gpui_kit::test]
fn idle_coordinator_wakes_for_filtered_edit_parks_on_duplicate_preview_and_updates_hud_on_tab_switch(
    cx: &mut TestAppContext,
) {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let path_a = dir_a.path().join("alpha.csv");
    let path_b = dir_b.path().join("beta.csv");
    std::fs::write(&path_a, "id,city\n1,Boston\n2,Boston\n3,Denver\n4,Denver\n").unwrap();
    std::fs::write(&path_b, "id,city\n10,Austin\n20,Seattle\n").unwrap();

    let controller = cx.update(|cx| {
        gpui_kit::init(cx);
        crate::apply_light_theme(cx);
        bind_shortcuts(cx);
        install_menus(cx);
        let entity = WorkbenchController::reset_for_test(
            cx,
            WorkbenchSchedulerConfig {
                index_cache_enabled: false,
                ..WorkbenchSchedulerConfig::default()
            },
        );
        WorkbenchController::start_coordinator_for_test(&entity, cx);
        entity
    });
    let (handle, view) = open_test_window(cx, &controller);
    let win_id = cx.update(|cx| view.read(cx).window_id);

    // Helper that drives ONLY the real spawned GPUI coordinator loop (without manually calling
    // `poll_background` from the test) so we verify genuine wake and park transitions.
    let wait_via_coordinator =
        |cx: &mut TestAppContext, pred: &dyn Fn(&QuarryWindow, &App) -> bool| {
            for _ in 0..200 {
                cx.background_executor
                    .advance_clock(std::time::Duration::from_millis(32));
                cx.run_until_parked();
                let done = cx
                    .update_window(handle.into(), |_, window, cx| {
                        let _ = window;
                        pred(view.read(cx), cx)
                    })
                    .unwrap();
                if done {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("real GPUI coordinator did not reach expected state in time");
        };

    // Open both files in separate directories and wait for indexing to settle cleanly.
    let (id_a, id_b) = cx.update(|cx| {
        let (_, id_a) = controller
            .update(cx, |c, cx| {
                c.open_paths(
                    win_id,
                    vec![path_a.clone()],
                    OpenOptions {
                        header_mode: HeaderMode::FirstRow,
                        ..OpenOptions::default()
                    },
                    None,
                    cx,
                )
            })
            .unwrap();
        let (_, id_b) = controller
            .update(cx, |c, cx| {
                c.open_paths(
                    win_id,
                    vec![path_b.clone()],
                    OpenOptions {
                        header_mode: HeaderMode::FirstRow,
                        ..OpenOptions::default()
                    },
                    None,
                    cx,
                )
            })
            .unwrap();
        (id_a, id_b)
    });
    wait_via_coordinator(cx, &|q, cx| {
        q.snapshot(cx).is_some_and(|s| s.index_done)
            && !controller.read(cx).has_active_background_work()
            && controller.read(cx).is_coordinator_parked()
    });
    assert!(
        cx.update(|cx| {
            controller
                .read(cx)
                .resource_hud_targets()
                .document_dirs
                .contains(dir_b.path())
        }),
        "active tab B directory must be registered in HUD targets"
    );

    // Idle tab switch back to tab A must immediately update HUD targets to dir_a.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.activate_tab(win_id, id_a, cx))
            .unwrap();
    });
    assert!(
        cx.update(|cx| {
            let targets = &controller.read(cx).resource_hud_targets().document_dirs;
            targets.contains(dir_a.path()) && !targets.contains(dir_b.path())
        }),
        "idle tab switch must update HUD targets to the newly active document's directory"
    );
    let _ = id_b;

    // Wait for tab A to settle and confirm the coordinator parks cleanly.
    wait_via_coordinator(cx, &|q, cx| {
        q.snapshot(cx).is_some_and(|s| s.index_done)
            && !controller.read(cx).has_active_background_work()
            && controller.read(cx).is_coordinator_parked()
    });

    // Apply a filter on city == Boston (2 matching rows), wait until parked, then edit one
    // matching cell and verify the parked coordinator wakes on its own and refreshes to 1 match.
    cx.update_window(handle.into(), |_, window, cx| {
        view.update(cx, |q, cx| {
            q.apply_quick_cell_filter(
                1,
                b"Boston".to_vec(),
                true,
                quarry_core::CaseSensitivity::Insensitive,
                window,
                cx,
            );
        });
    })
    .unwrap();
    wait_via_coordinator(cx, &|q, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.filter_active && s.filter_done && s.available_rows == 2)
            && !controller.read(cx).has_active_background_work()
            && controller.read(cx).is_coordinator_parked()
    });

    let active_after_edit = cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.apply_cell_edit(win_id, id_a, 1, 1, "Miami".into(), cx)
            })
            .unwrap();
        controller.read(cx).has_active_background_work()
    });
    assert!(
        active_after_edit,
        "editing a cell while filtered must report active background work so a parked coordinator wakes to refresh matches"
    );
    wait_via_coordinator(cx, &|q, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.filter_active && s.filter_done && s.available_rows == 1)
            && !controller.read(cx).has_active_background_work()
            && controller.read(cx).is_coordinator_parked()
    });

    // Clear filter, run Find Duplicates preview, and verify that once the confirmation modal is
    // ready (`StructuralJob::DuplicatePreview`), the real coordinator parks without polling while
    // waiting on user confirmation.
    cx.update(|cx| {
        controller.update(cx, |c, cx| {
            c.clear_filter_active(win_id, cx).unwrap();
            c.start_find_duplicates_active(
                win_id,
                id_a,
                quarry_core::DuplicateTarget::Columns(vec![1]),
                quarry_core::CaseSensitivity::Insensitive,
                cx,
            )
            .unwrap();
        });
    });
    wait_via_coordinator(cx, &|q, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.duplicate_preview.is_some())
            && !controller.read(cx).has_active_background_work()
            && controller.read(cx).is_coordinator_parked()
    });
    assert!(
        cx.update(|cx| {
            controller
                .read(cx)
                .active_snapshot(win_id)
                .is_some_and(|s| s.duplicate_preview.is_some())
                && !controller.read(cx).has_active_background_work()
                && controller.read(cx).is_coordinator_parked()
        }),
        "completed DuplicatePreview waiting on user confirmation must park the real coordinator"
    );
}

#[gpui_kit::test]
fn query_bar_applies_syncs_rules_isolates_tabs_and_preserves_active_filter_on_error(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("orders.csv");
    let path_b = dir.path().join("other.csv");
    fs::write(
        &path_a,
        "state,order total,code,tag\n\
         TX,25,ERR-007,d1\n\
         FL,100,ERR-042,d2\n\
         CA,50,ERR-009,d3\n\
         TX,150,ERR-099,d4\n\
         FL,10,OK-001,d5\n\
         TX,25,ERR-008,d6\n",
    )
    .unwrap();
    fs::write(&path_b, "region,score\nwest,10\neast,20\n").unwrap();

    let (win, view, controller, win_id) = open_test_window(cx);
    let (id_a, id_b) = cx.update(|cx| {
        let a = controller
            .update(cx, |c, cx| c.open_file(win_id, &path_a, cx))
            .unwrap();
        let b = controller
            .update(cx, |c, cx| c.open_file(win_id, &path_b, cx))
            .unwrap();
        controller
            .update(cx, |c, cx| c.activate_tab(win_id, a, cx))
            .unwrap();
        (a, b)
    });
    wait_until(cx, win, &view, &controller, |q, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.document_id == id_a && s.index_done && s.total_rows == 6)
    });

    // Open Query Bar, switch to Enter-to-apply, type a compound expression, and apply.
    win.update(cx, |_, window, cx| {
        view.update(cx, |q, cx| {
            q.toggle_query_bar_action(&ToggleQueryBar, window, cx);
            assert!(q.query_bar.read(cx).is_open());
            assert!(
                q.query_bar.read(cx).live_mode_for_test(),
                "small files (<= 10 MiB) default to live mode enabled"
            );
            q.query_bar.update(cx, |bar, cx| {
                bar.set_live_mode_for_test(false, window, cx);
                bar.set_match_case_for_test(true, cx);
                bar.set_expression_for_test(
                    "state == \"TX\" OR state == \"FL\" AND [order total] >= 10 AND [order total] <= 100 AND code =~ /^ERR-\\d+$/ AND #4 != \"d6\"",
                    window,
                    cx,
                );
                assert!(bar.parse_error_for_test().is_none());
                bar.apply_now(cx);
            });
        });
    })
    .unwrap();

    wait_until(cx, win, &view, &controller, |q, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.filter_active && s.filter_done && s.available_rows == 2)
    });

    // Verify canonical expression synced back to Query Bar AND two-way sync populated Filters rules
    // (including Between folding and Matches regex operator).
    cx.update(|cx| {
        let q = view.read(cx);
        let text = q.query_bar.read(cx).text_for_test(cx);
        assert_eq!(
            text,
            "state == \"TX\" OR state == \"FL\" AND [order total] BETWEEN 10 AND 100 AND code =~ \"^ERR-\\\\d+$\" AND tag != \"d6\""
        );
        let synced = q.panels.read(cx).synced_query_for_test(cx);
        assert_eq!(synced.predicates.len(), 5);
        assert_eq!(synced.predicates[2].operator, quarry_core::FilterOperator::Between);
        assert_eq!(synced.predicates[3].operator, quarry_core::FilterOperator::Regex);
        assert_eq!(synced.case_sensitivity, quarry_core::CaseSensitivity::Sensitive);
    });

    // Typing an unrepresentable expression (cross-column OR) shows inline diagnostic WITHOUT
    // disturbing the 2 active filtered rows.
    win.update(cx, |_, window, cx| {
        view.update(cx, |q, cx| {
            q.query_bar.update(cx, |bar, cx| {
                bar.set_expression_for_test("state == \"TX\" OR [order total] > 10", window, cx);
                let err = bar
                    .parse_error_for_test()
                    .expect("cross-column OR must report inline diagnostic");
                assert!(err.message.contains("same column"));
                bar.apply_now(cx);
            });
        });
    })
    .unwrap();
    wait_until(cx, win, &view, &controller, |q, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.filter_active && s.filter_done && s.available_rows == 2)
    });

    // Switching to Tab B isolates Query Bar draft/errors; switching back restores Tab A's draft.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.activate_tab(win_id, id_b, cx))
            .unwrap();
    });
    wait_until(cx, win, &view, &controller, |q, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.document_id == id_b && s.index_done && s.total_rows == 2)
    });
    cx.update(|cx| {
        let bar = view.read(cx).query_bar.read(cx);
        assert_eq!(bar.text_for_test(cx), "");
        assert!(bar.parse_error_for_test().is_none());
    });

    // Applying a filter on Tab B via controller syncs canonical expression into Query Bar.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| {
                c.start_filter_active(
                    win_id,
                    quarry_core::FilterQuery::single(
                        0,
                        quarry_core::FilterOperator::Equals,
                        b"west".to_vec(),
                    ),
                    cx,
                )
                .unwrap();
            })
            .unwrap();
    });
    wait_until(cx, win, &view, &controller, |q, cx| {
        q.snapshot(cx).is_some_and(|s| {
            s.document_id == id_b && s.filter_active && s.filter_done && s.available_rows == 1
        })
    });
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).query_bar.read(cx).text_for_test(cx),
            "region == \"west\""
        );
    });

    // Switch back to Tab A: its in-progress invalid draft is preserved, and Clear restores all 6 rows.
    cx.update(|cx| {
        controller
            .update(cx, |c, cx| c.activate_tab(win_id, id_a, cx))
            .unwrap();
    });
    wait_until(cx, win, &view, &controller, |q, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.document_id == id_a && s.filter_active && s.available_rows == 2)
    });
    win.update(cx, |_, window, cx| {
        view.update(cx, |q, cx| {
            assert!(q.query_bar.read(cx).parse_error_for_test().is_some());
            q.query_bar.update(cx, |bar, cx| bar.clear_now(window, cx));
        });
    })
    .unwrap();
    wait_until(cx, win, &view, &controller, |q, cx| {
        q.snapshot(cx)
            .is_some_and(|s| s.document_id == id_a && !s.filter_active && s.available_rows == 6)
    });
    cx.update(|cx| {
        let q = view.read(cx);
        assert_eq!(q.query_bar.read(cx).text_for_test(cx), "");
        assert!(q.query_bar.read(cx).parse_error_for_test().is_none());
    });
}
