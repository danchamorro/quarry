use quarry_core::{
    ByteWindow, FilterOperator, FilterQuery, HeaderMode, OpenOptions, SectionOpenOptions,
    WatchAlert, WatchConfig, WatchStatus,
};
use quarry_workbench::{
    CommandContext, CommandId, Document, LIVE_WATCH_READ_ONLY_REASON, PinReason, WakeOutcome,
    WindowId, WorkbenchScheduler, WorkbenchSchedulerConfig, WorkbenchTab, evaluate_command,
};
use std::fs::OpenOptions as FsOpenOptions;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

fn wait_until<F>(timeout: Duration, mut pred: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        thread::sleep(Duration::from_millis(15));
    }
    pred()
}

fn open_completed_scheduler(path: &Path) -> (WorkbenchScheduler, quarry_workbench::DocumentId) {
    let dir = path.parent().unwrap().join("scratch");
    std::fs::create_dir_all(&dir).unwrap();
    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        working_directory: dir,
        index_cache_enabled: false,
        ..WorkbenchSchedulerConfig::default()
    });
    scheduler.register_window(WindowId::PRIMARY);
    let doc_id = match scheduler
        .open_path(
            WindowId::PRIMARY,
            path.to_path_buf(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
            None,
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. }
        | WakeOutcome::SourceChangedConflict { document_id, .. } => document_id,
    };
    assert!(wait_until(Duration::from_secs(3), || {
        let _ = scheduler.poll_all();
        scheduler
            .tab(doc_id)
            .and_then(|tab| tab.document.as_ref())
            .is_some_and(|doc| doc.index.as_ref().is_some_and(|idx| idx.is_complete()))
    }));
    (scheduler, doc_id)
}

#[test]
fn live_watch_pins_tab_accounts_fds_blocks_edits_and_pauses_tail_on_scroll_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stream.csv");
    let mut initial = String::from("id,msg\n");
    for i in 1..=60 {
        initial.push_str(&format!("{i},row-{i:03}\n"));
    }
    std::fs::write(&path, &initial).unwrap();

    let (mut scheduler, doc_id) = open_completed_scheduler(&path);
    let baseline_fds = scheduler.fd_budget().snapshot().total_reserved;

    // Eligible clean complete document: ToggleLiveWatch command enabled.
    {
        let tab = scheduler.tab(doc_id).unwrap();
        let ctx = CommandContext {
            window: WindowId::PRIMARY,
            active_tab: tab,
            open_document_count: 1,
            write_slot_available: true,
            external_modal_blocked: false,
            text_field_focused: false,
            show_all_whitespace: false,
        };
        assert!(evaluate_command(CommandId::ToggleLiveWatch, &ctx).enabled);
    }

    // Start Live Watch and verify pinning + FD budget reservation + mutual exclusion with edits.
    scheduler
        .start_live_watch_with_config(
            doc_id,
            WatchConfig {
                poll_interval: Duration::from_millis(15),
                cache_config: quarry_core::IndexCacheConfig::disabled(),
                ..Default::default()
            },
        )
        .unwrap();

    // Open a second tab so doc_id is no longer ActiveInWindow -> must still be pinned as LiveWatching!
    let other_path = dir.path().join("other.csv");
    std::fs::write(&other_path, b"a,b\n1,2\n").unwrap();
    let other_id = match scheduler
        .open_path(WindowId::PRIMARY, other_path, OpenOptions::default(), None)
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. }
        | WakeOutcome::SourceChangedConflict { document_id, .. } => document_id,
    };
    assert_eq!(scheduler.pin_reason(doc_id), Some(PinReason::LiveWatching));

    // Switch back to the watched tab and check FD budget + edit/save/structural exclusion.
    let _ = scheduler.activate_tab(WindowId::PRIMARY, doc_id);
    assert!(scheduler.fd_budget().snapshot().total_reserved > baseline_fds);
    {
        let tab = scheduler.tab(doc_id).unwrap();
        let doc = tab.document.as_ref().unwrap();
        assert!(doc.is_live_watching());
        assert_eq!(
            doc.cell_edit_disabled_reason(Some(b"row-001")),
            Some(LIVE_WATCH_READ_ONLY_REASON)
        );
        assert!(!doc.header_is_editable(0));
        assert_eq!(
            doc.structural_edit_disabled_reason(),
            Some(LIVE_WATCH_READ_ONLY_REASON)
        );
        assert!(!doc.is_save_ready());
        assert!(!doc.is_save_as_ready());
        assert!(!doc.is_search_ready());
    }

    // Scroll up: tail-follow pauses, viewport_start stays pinned while ingestion continues!
    {
        let doc = scheduler
            .tab_mut(doc_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.set_visible_rows(10).unwrap();
        doc.scroll_rows(-20).unwrap();
        assert!(!doc.live_watch_tail_follow);
    }
    let anchored_viewport = scheduler
        .tab(doc_id)
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .viewport_start;

    let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
    for i in 61..=100 {
        writeln!(writer, "{i},row-{i:03}").unwrap();
    }
    writer.flush().unwrap();
    scheduler
        .tab(doc_id)
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .live_watch
        .as_ref()
        .unwrap()
        .nudge();

    assert!(wait_until(Duration::from_secs(3), || {
        let _ = scheduler.poll_all();
        scheduler
            .tab(doc_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .available_data_rows()
            == 100
    }));

    // Scroll position stayed rock-solid while 40 rows were ingested!
    {
        let doc = scheduler.tab(doc_id).unwrap().document.as_ref().unwrap();
        assert!(!doc.live_watch_tail_follow);
        assert_eq!(doc.viewport_start, anchored_viewport);
    }

    // Resume tail-follow -> jumps cleanly to the bottom tail (row 91..=100).
    {
        let doc = scheduler
            .tab_mut(doc_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.set_live_watch_tail_follow(true).unwrap();
        assert!(doc.live_watch_tail_follow);
        assert_eq!(doc.viewport_start, 91);
    }

    // Clean stop returns full editing/saving capabilities and releases watcher FDs.
    let alert = scheduler.stop_live_watch(doc_id).unwrap();
    assert!(alert.is_none());
    let _ = scheduler.close_tab(other_id);
    assert_eq!(
        scheduler.fd_budget().snapshot().total_reserved,
        baseline_fds
    );
    let doc = scheduler.tab(doc_id).unwrap().document.as_ref().unwrap();
    assert!(!doc.is_live_watching());
    assert!(!doc.source_changed);
    assert!(doc.cell_edit_disabled_reason(Some(b"row-100")).is_none());
    assert!(doc.header_is_editable(0));
    assert!(doc.is_search_ready());
}

#[test]
fn live_watch_rejects_dirty_and_section_documents_and_surfaces_rotation_alert() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rotate.csv");
    std::fs::write(&path, b"id,level\n1,INFO\n2,WARN\n").unwrap();

    // 1. Section document must be rejected.
    let section_doc = Document::prepare_section(
        &path,
        ByteWindow::new(0, 20).unwrap(),
        SectionOpenOptions::default(),
    )
    .unwrap();
    assert!(section_doc.live_watch_unavailable_reason().is_some());
    assert!(!section_doc.can_start_live_watch());

    // 2. Dirty document must be rejected.
    let (mut scheduler, doc_id) = open_completed_scheduler(&path);
    {
        let doc = scheduler
            .tab_mut(doc_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.begin_cell_edit(1, 1, b"INFO".to_vec()).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "EDITED".into();
        doc.commit_cell_edit();
        assert!(doc.is_dirty());
        assert!(!doc.can_start_live_watch());
        assert!(doc.live_watch_unavailable_reason().is_some());
        doc.discard_edits();
    }

    // 3. Active filter updates incrementally during watch, then rename rotation triggers Alert(Rotated).
    {
        let doc = scheduler
            .tab_mut(doc_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.start_filter(FilterQuery::single(
            1,
            FilterOperator::Equals,
            b"WARN".to_vec(),
        ))
        .unwrap();
    }
    assert!(wait_until(Duration::from_secs(2), || {
        let _ = scheduler.poll_all();
        scheduler
            .tab(doc_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .filter_job
            .is_none()
    }));

    scheduler
        .start_live_watch_with_config(
            doc_id,
            WatchConfig {
                poll_interval: Duration::from_millis(15),
                cache_config: quarry_core::IndexCacheConfig::disabled(),
                ..Default::default()
            },
        )
        .unwrap();

    let mut old_writer = FsOpenOptions::new().append(true).open(&path).unwrap();
    old_writer.write_all(b"3,WARN\n4,INFO\n").unwrap();
    old_writer.flush().unwrap();
    scheduler
        .tab(doc_id)
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .live_watch
        .as_ref()
        .unwrap()
        .nudge();

    assert!(wait_until(Duration::from_secs(2), || {
        let _ = scheduler.poll_all();
        scheduler
            .tab(doc_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .available_filter_rows()
            == 2
    }));

    // Rotate the file: rename away and create a new file at the same path.
    std::fs::rename(&path, dir.path().join("rotate.csv.1")).unwrap();
    std::fs::write(&path, b"id,level\n10,WARN\n").unwrap();
    if let Some(watch) = scheduler
        .tab(doc_id)
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .live_watch
        .as_ref()
    {
        watch.nudge();
    }

    assert!(wait_until(Duration::from_secs(2), || {
        let _ = scheduler.poll_all();
        scheduler
            .tab(doc_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .live_watch_alert
            == Some(WatchAlert::Rotated)
    }));

    let doc = scheduler.tab(doc_id).unwrap().document.as_ref().unwrap();
    assert!(!doc.is_live_watching());
    assert!(doc.source_changed);
    assert_eq!(
        doc.live_watch_snapshot.as_ref().map(|s| &s.status),
        Some(&WatchStatus::Alert(WatchAlert::Rotated))
    );
    let _ = WorkbenchTab::new(1);
}
