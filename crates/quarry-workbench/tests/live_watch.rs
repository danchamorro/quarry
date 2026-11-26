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

#[test]
fn live_watch_unterminated_baseline_rollback_reports_zero_then_one_and_blocks_reopen_and_bookmarks()
{
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unterminated_wb.csv");
    // Header + 1 unterminated row (no trailing newline).
    std::fs::write(&path, b"id,val\n1,par").unwrap();

    let (mut scheduler, doc_id) = open_completed_scheduler(&path);
    assert_eq!(
        scheduler
            .tab(doc_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .available_data_rows(),
        1
    );

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

    // Unterminated baseline row rolled back -> authoritative `available_data_rows()` MUST be 0!
    {
        let doc = scheduler.tab(doc_id).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.available_data_rows(), 0);
        assert!(doc.split_at_bookmarks_disabled_reason().is_some());
    }

    // Reopen tab (Format/encoding change) MUST be blocked while Live Watch is active!
    assert!(
        scheduler
            .reopen_tab(WindowId::PRIMARY, doc_id, OpenOptions::default(), None)
            .is_err()
    );

    // Finish the unterminated row -> `available_data_rows()` becomes 1 cleanly!
    let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
    writer.write_all(b"tial\n").unwrap();
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

    assert!(wait_until(Duration::from_secs(2), || {
        let _ = scheduler.poll_all();
        scheduler
            .tab(doc_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .available_data_rows()
            == 1
    }));

    // Overwrite row 0 in place with malformed unclosed-quote CSV -> `poll_all` must surface
    // `WatchAlert::Rewritten` and set `source_changed = true` rather than swallowing the error!
    let mut rw = FsOpenOptions::new().write(true).open(&path).unwrap();
    rw.write_all(b"\"broken\n1,partial\n").unwrap();
    rw.flush().unwrap();
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
            == Some(WatchAlert::Rewritten)
    }));
    let doc = scheduler.tab(doc_id).unwrap().document.as_ref().unwrap();
    assert!(!doc.is_live_watching());
    assert!(doc.source_changed);
}

#[test]
fn stop_live_watch_reconciles_filter_query_when_cancelling_in_flight_replacement_filter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("filter_reconcile.csv");
    let mut content = String::from("id,level,payload\n1,WARN,seed\n");
    for i in 2..=50_000 {
        content.push_str(&format!("{i},INFO,payload-{i:06}\n"));
    }
    std::fs::write(&path, &content).unwrap();

    let (mut scheduler, doc_id) = open_completed_scheduler(&path);
    let warn_query = FilterQuery::single(1, FilterOperator::Equals, b"WARN".to_vec());
    let info_query = FilterQuery::single(1, FilterOperator::Equals, b"INFO".to_vec());

    // Start and complete Filter A (WARN).
    scheduler
        .tab_mut(doc_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .start_filter(warn_query.clone())
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || {
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
                poll_interval: Duration::from_millis(50),
                cache_config: quarry_core::IndexCacheConfig::disabled(),
                ..Default::default()
            },
        )
        .unwrap();

    // Start Filter B (INFO) while watching and immediately Stop Watch (cancelling or completing B).
    scheduler
        .tab_mut(doc_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .start_filter(info_query)
        .unwrap();
    let _ = scheduler.stop_live_watch(doc_id).unwrap();

    // Regardless of whether B finished before cancellation or was cancelled mid-attach,
    // `doc.filter_query` MUST match `doc.filter_index.query()`!
    let doc = scheduler.tab(doc_id).unwrap().document.as_ref().unwrap();
    let idx = doc.filter_index.as_ref().expect("filter index retained");
    assert_eq!(doc.filter_query.as_ref(), Some(idx.query()));
    if idx.query() == &warn_query {
        assert_eq!(idx.matches_found(), 1);
    } else {
        assert_eq!(idx.matches_found(), 49_999);
    }
}

#[test]
fn live_watch_filter_attach_completion_preserves_concurrently_appended_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attach_race.csv");
    std::fs::write(&path, b"id,level\n1,WARN\n2,INFO\n").unwrap();

    let (mut scheduler, doc_id) = open_completed_scheduler(&path);
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

    let query = FilterQuery::single(1, FilterOperator::Equals, b"WARN".to_vec());
    scheduler
        .tab_mut(doc_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .start_filter(query)
        .unwrap();

    // Wait for the attachment background worker to finish installing the filter into the watcher
    // WITHOUT calling `poll_filter_scan` yet.
    assert!(wait_until(Duration::from_secs(2), || {
        scheduler
            .tab(doc_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .live_watch_filter_attach
            .as_ref()
            .is_some_and(|job| job.is_done())
    }));

    // Append a second matching row AFTER filter installation and let the watcher ingest it.
    let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
    writer.write_all(b"3,WARN\n").unwrap();
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
    assert!(wait_until(Duration::from_secs(2), || {
        scheduler
            .tab(doc_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .live_watch
            .as_ref()
            .unwrap()
            .snapshot()
            .filter_matches_found
            == Some(2)
    }));

    // Now run `poll_all()` (which runs `poll_live_watch` first, syncing 2 matches, and then
    // collects `live_watch_filter_attach` whose returned worker snapshot only had 1 match).
    assert!(scheduler.poll_all().is_empty());

    let doc = scheduler.tab(doc_id).unwrap().document.as_ref().unwrap();
    assert!(doc.live_watch_filter_attach.is_none());
    assert_eq!(
        doc.filter_index.as_ref().map(|idx| idx.matches_found()),
        Some(2),
        "collecting the completed attach job must not regress matches from 2 back to 1"
    );
    let _ = scheduler.stop_live_watch(doc_id).unwrap();
}

#[test]
fn live_watch_tab_stays_pinned_when_replacing_active_tab_under_hydration_limit() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let path_a = dir.path().join("watched.csv");
    let path_b = dir.path().join("other.csv");
    std::fs::write(&path_a, b"id,v\n1,a\n").unwrap();
    std::fs::write(&path_b, b"id,v\n1,b\n").unwrap();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        working_directory: scratch,
        index_cache_enabled: false,
        max_hydrated_tabs: 1,
        ..WorkbenchSchedulerConfig::default()
    });
    scheduler.register_window(WindowId::PRIMARY);
    let doc_a = match scheduler
        .open_path(WindowId::PRIMARY, path_a, OpenOptions::default(), None)
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. }
        | WakeOutcome::SourceChangedConflict { document_id, .. } => document_id,
    };
    assert!(wait_until(Duration::from_secs(3), || {
        let _ = scheduler.poll_all();
        scheduler
            .tab(doc_a)
            .and_then(|tab| tab.document.as_ref())
            .is_some_and(|doc| doc.index.as_ref().is_some_and(|idx| idx.is_complete()))
    }));

    scheduler
        .start_live_watch_with_config(
            doc_a,
            WatchConfig {
                poll_interval: Duration::from_millis(50),
                cache_config: quarry_core::IndexCacheConfig::disabled(),
                ..Default::default()
            },
        )
        .unwrap();

    // Opening a second tab under `max_hydrated_tabs = 1` must recognize `LiveWatching` as a
    // non-active pin and refuse to evict `doc_a` (`AllTabsPinned`), keeping `doc_a` live.
    assert!(matches!(
        scheduler.open_path(WindowId::PRIMARY, path_b, OpenOptions::default(), None),
        Err(quarry_workbench::SchedulerError::AllTabsPinned { .. })
    ));
    assert!(
        scheduler
            .tab(doc_a)
            .unwrap()
            .document
            .as_ref()
            .is_some_and(|doc| doc.is_live_watching()),
        "live-watching tab must remain hydrated and watching when replacing active tab is refused"
    );
    let _ = scheduler.stop_live_watch(doc_a).unwrap();
}

#[test]
fn live_watch_refreshes_headers_when_unterminated_header_row_commits_and_after_stop() {
    // Case 1: unterminated header expands from 1 column ("sta") to 2 columns ("status", "detail").
    let dir = tempfile::tempdir().unwrap();
    let path1 = dir.path().join("partial_header_grow.csv");
    std::fs::write(&path1, b"sta").unwrap();

    let (mut scheduler, doc1) = open_completed_scheduler(&path1);
    assert_eq!(
        scheduler
            .tab(doc1)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .headers,
        vec!["sta"]
    );
    scheduler
        .start_live_watch_with_config(
            doc1,
            WatchConfig {
                poll_interval: Duration::from_millis(15),
                cache_config: quarry_core::IndexCacheConfig::disabled(),
                ..Default::default()
            },
        )
        .unwrap();

    let mut f1 = FsOpenOptions::new().append(true).open(&path1).unwrap();
    f1.write_all(b"tus,detail\nOK,info\n").unwrap();
    f1.flush().unwrap();
    scheduler
        .tab(doc1)
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
            .tab(doc1)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .headers
            == vec!["status", "detail"]
    }));
    let _ = scheduler.stop_live_watch(doc1).unwrap();
    assert_eq!(
        scheduler
            .tab(doc1)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .headers,
        vec!["status", "detail"]
    );

    // Case 2: unterminated header stays at the same column count ("id", "st" -> "id", "status").
    let path2 = dir.path().join("partial_header_same_width.csv");
    std::fs::write(&path2, b"id,st").unwrap();
    let (mut scheduler2, doc2) = open_completed_scheduler(&path2);
    assert_eq!(
        scheduler2
            .tab(doc2)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .headers,
        vec!["id", "st"]
    );
    scheduler2
        .start_live_watch_with_config(
            doc2,
            WatchConfig {
                poll_interval: Duration::from_millis(15),
                cache_config: quarry_core::IndexCacheConfig::disabled(),
                ..Default::default()
            },
        )
        .unwrap();

    let mut f2 = FsOpenOptions::new().append(true).open(&path2).unwrap();
    f2.write_all(b"atus\n1,ok\n").unwrap();
    f2.flush().unwrap();
    scheduler2
        .tab(doc2)
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .live_watch
        .as_ref()
        .unwrap()
        .nudge();

    assert!(wait_until(Duration::from_secs(3), || {
        let _ = scheduler2.poll_all();
        scheduler2
            .tab(doc2)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .headers
            == vec!["id", "status"]
    }));
    let _ = scheduler2.stop_live_watch(doc2).unwrap();
    assert_eq!(
        scheduler2
            .tab(doc2)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .headers,
        vec!["id", "status"]
    );
}

#[test]
fn live_watch_prepare_async_buffer_read_returns_background_plan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("async_viewport.csv");
    let mut content = String::from("id,msg\n");
    for i in 1..=300 {
        content.push_str(&format!("{i},row-{i:04}\n"));
    }
    std::fs::write(&path, content.as_bytes()).unwrap();

    let (mut scheduler, doc_id) = open_completed_scheduler(&path);
    scheduler
        .start_live_watch_with_config(
            doc_id,
            WatchConfig {
                poll_interval: Duration::from_millis(50),
                cache_config: quarry_core::IndexCacheConfig::disabled(),
                ..Default::default()
            },
        )
        .unwrap();

    // Request an uncached page near row 0 while watching (buffer currently sits at tail).
    let plan = scheduler
        .tab_mut(doc_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .prepare_async_buffer_read(0, 20)
        .unwrap()
        .expect("live watch uncached viewport must return an off-UI-thread ViewportReadPlan");
    assert!(plan.live_watch_reader.is_some());
    let rows = plan.execute().unwrap();
    assert!(!rows.is_empty());
    assert_eq!(rows[0].fields[0], b"1");
    let _ = scheduler.stop_live_watch(doc_id).unwrap();
}

#[test]
fn live_watch_filtered_page_reads_run_in_background_worker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("async_filtered.csv");
    let mut content = String::from("id,level\n");
    for i in 1..=200 {
        let level = if i % 10 == 0 { "ERR" } else { "INFO" };
        content.push_str(&format!("{i},{level}\n"));
    }
    std::fs::write(&path, content.as_bytes()).unwrap();

    let (mut scheduler, doc_id) = open_completed_scheduler(&path);
    scheduler
        .start_live_watch_with_config(
            doc_id,
            WatchConfig {
                poll_interval: Duration::from_millis(50),
                cache_config: quarry_core::IndexCacheConfig::disabled(),
                ..Default::default()
            },
        )
        .unwrap();

    let query = FilterQuery::single(1, FilterOperator::Equals, b"ERR".to_vec());
    scheduler
        .tab_mut(doc_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .start_filter(query)
        .unwrap();

    // Wait for filter attachment and background filter page read to populate rows.
    assert!(wait_until(Duration::from_secs(3), || {
        let _ = scheduler.poll_all();
        let doc = scheduler.tab(doc_id).unwrap().document.as_ref().unwrap();
        doc.live_watch_filter_attach.is_none()
            && doc.filter_index.as_ref().map(|idx| idx.matches_found()) == Some(20)
            && !doc.filtered_rows.is_empty()
    }));

    // Clear the cached filtered buffer and request page 0 -> must schedule a background FilterReadJob.
    {
        let doc = scheduler
            .tab_mut(doc_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.filtered_rows.clear();
        doc.navigate_filter(0).unwrap();
        assert!(
            doc.filter_read.is_some(),
            "watched filter-page reads must schedule a background FilterReadJob"
        );
    }

    assert!(wait_until(Duration::from_secs(3), || {
        let _ = scheduler.poll_all();
        let doc = scheduler.tab(doc_id).unwrap().document.as_ref().unwrap();
        doc.filter_read.is_none() && !doc.filtered_rows.is_empty()
    }));
    let doc = scheduler.tab(doc_id).unwrap().document.as_ref().unwrap();
    assert_eq!(doc.filtered_rows[0].fields[0], b"10");
    let _ = scheduler.stop_live_watch(doc_id).unwrap();
}
