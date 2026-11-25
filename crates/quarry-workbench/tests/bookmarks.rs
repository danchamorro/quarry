use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use quarry_core::{
    BoundaryTrust, ByteWindow, CaseSensitivity, DuplicateTarget, FilterOperator, FilterQuery,
    HeaderMode, OpenOptions, SectionOpenOptions, SortDirection, SortMode, SortPlan, SortSpec,
    VerifyBoundaryOutcome,
};
use quarry_workbench::{
    CommandContext, CommandId, DocumentId, ResidencyState, StructuralDialog, WakeOutcome, WindowId,
    WorkbenchScheduler, WorkbenchSchedulerConfig, evaluate_command,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_csv(name: &str, bytes: &[u8]) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "quarry-wb-bookmarks-{}-{}-{}.csv",
        std::process::id(),
        id,
        name
    ));
    let mut f = File::create(&path).unwrap();
    f.write_all(bytes).unwrap();
    f.flush().unwrap();
    path
}

fn open_options() -> OpenOptions {
    OpenOptions {
        header_mode: HeaderMode::FirstRow,
        ..Default::default()
    }
}

fn wait_for_index(scheduler: &mut WorkbenchScheduler, id: DocumentId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let errs = scheduler.poll_all();
        assert!(errs.is_empty(), "unexpected indexing errors: {errs:?}");
        if scheduler
            .tab(id)
            .and_then(|t| t.document.as_ref())
            .is_some_and(|d| d.job.is_none() && d.progress.done && d.bookmark_offset_job.is_none())
        {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for index");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_filter(scheduler: &mut WorkbenchScheduler, id: DocumentId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let errs = scheduler.poll_all();
        assert!(errs.is_empty(), "unexpected filter errors: {errs:?}");
        if scheduler
            .tab(id)
            .and_then(|t| t.document.as_ref())
            .is_some_and(|d| d.filter_job.is_none() && !d.filter_rows_loading())
        {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for filter");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_structural(
    scheduler: &mut WorkbenchScheduler,
    id: DocumentId,
) -> Result<Option<String>, quarry_workbench::AppMessage> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let _ = scheduler.poll_all();
        for completion in scheduler.poll_structural_edits() {
            if completion.document_id == id {
                return completion.result;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for structural edit"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_save(scheduler: &mut WorkbenchScheduler, id: DocumentId) -> (PathBuf, bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let _ = scheduler.poll_all();
        for completion in scheduler.poll_saves() {
            if completion.document_id == id {
                return completion.result.expect("save should succeed");
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for save");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_verify(
    scheduler: &mut WorkbenchScheduler,
    id: DocumentId,
) -> quarry_workbench::SectionVerifyCompletion {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        for completion in scheduler.poll_section_verifications() {
            if completion.document_id == id {
                return completion;
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for verify");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn bookmarks_keyed_in_whole_files_and_speculative_vs_verified_sections() {
    // Write > 17 MiB so opening a section past 16 MiB without a parent index starts Speculative.
    let mut bytes = Vec::from(&b"id,payload\n"[..]);
    let pad = "p".repeat(60);
    let mut row_idx = 0_u64;
    while bytes.len() < 17 * 1024 * 1024 + 8192 {
        row_idx += 1;
        bytes.extend_from_slice(format!("{row_idx},{pad}\n").as_bytes());
    }
    let path = temp_csv("speculative-verify", &bytes);
    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        index_cache_enabled: false,
        ..Default::default()
    });
    scheduler.register_window(WindowId::PRIMARY);

    let sec_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path.clone(),
            ByteWindow::from_range(16 * 1024 * 1024 + 500, 16 * 1024 * 1024 + 4096).unwrap(),
            SectionOpenOptions {
                open: open_options(),
                ..Default::default()
            },
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open_section outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, sec_id);

    // Speculative section: bookmarks work for navigation, global_ordinal is None, and Split-at-Bookmarks is disabled.
    {
        let doc = scheduler
            .tab_mut(sec_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        assert_eq!(doc.section_trust(), Some(BoundaryTrust::Speculative));
        assert!(doc.toggle_bookmark_row(0).unwrap());
        assert!(doc.toggle_bookmark_row(2).unwrap());
        let bm0 = doc.bookmarks.get(0).unwrap();
        let bm2 = *doc.bookmarks.get(2).unwrap();
        assert_eq!(bm0.key.source_generation, 1);
        assert!(bm0.key.record_start_offset.is_some());
        assert_eq!(bm0.key.global_ordinal, None);
        assert_eq!(bm2.key.global_ordinal, None);
        assert!(doc.bookmark_row_label(&bm2).contains("global row unknown"));
        let reason = doc
            .split_at_bookmarks_disabled_reason()
            .expect("split at bookmarks must be disabled on Speculative sections");
        assert!(reason.contains("speculative"));
        assert!(doc.bookmark_split_targets().is_err());
    }

    // Verify Boundary upgrades trust in place (no boundary shift on clean unquoted rows) and backfills global ordinals.
    scheduler.start_verify_section_boundary(sec_id).unwrap();
    let verify = wait_for_verify(&mut scheduler, sec_id);
    assert!(matches!(
        verify.result,
        Ok(VerifyBoundaryOutcome::Verified {
            boundary_changed: false,
            ..
        })
    ));

    {
        let doc = scheduler
            .tab_mut(sec_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        assert_eq!(doc.section_trust(), Some(BoundaryTrust::Verified));
        let bm2 = *doc.bookmarks.get(2).unwrap();
        assert!(
            bm2.key.global_ordinal.is_some(),
            "Verify Boundary must backfill global_ordinal on existing bookmarks"
        );
        assert_eq!(doc.split_at_bookmarks_disabled_reason(), None);
        // Split targets drop local_row 0 (first data row in interior section) and retain row 2.
        let targets = doc.bookmark_split_targets().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].local_row, 2);
        assert_eq!(
            Some(targets[0].record_start_offset),
            bm2.key.record_start_offset
        );
        assert_eq!(targets[0].global_ordinal, bm2.key.global_ordinal);
    }

    let _ = fs::remove_file(&path);
}

#[test]
fn bookmarks_preserved_across_cell_edits_filters_and_dormancy() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("first.csv");
    let p2 = dir.path().join("second.csv");
    fs::write(
        &p1,
        b"id,name,group\n1,alpha,a\n2,beta,b\n3,gamma,a\n4,delta,b\n5,epsilon,a\n",
    )
    .unwrap();
    fs::write(&p2, b"id,name\n1,other\n").unwrap();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    scheduler.register_window(WindowId::PRIMARY);
    let ids = scheduler
        .open_many_lazy(
            WindowId::PRIMARY,
            [p1.clone(), p2.clone()],
            open_options(),
            None,
        )
        .unwrap();
    wait_for_index(&mut scheduler, ids[0]);

    // Bookmark rows 2 (beta) and 5 (epsilon) and verify exact byte offsets.
    let (orig_gen, offset_2, offset_5) = {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        assert!(doc.toggle_bookmark_row(2).unwrap());
        assert!(doc.toggle_bookmark_row(5).unwrap());
        // Adding bookmarks does not dirty the document.
        assert!(!doc.is_dirty());
        let bm2 = *doc.bookmarks.get(2).unwrap();
        let bm5 = doc.bookmarks.get(5).unwrap();
        assert_eq!(bm2.key.global_ordinal, Some(2));
        assert_eq!(bm5.key.global_ordinal, Some(5));
        let off2 = bm2.key.record_start_offset.unwrap();
        let off5 = bm5.key.record_start_offset.unwrap();
        assert!(off2 < off5);
        (doc.source_generation, off2, off5)
    };

    // 1. Clean dormancy preserves bookmarks verbatim.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantClean)
    );
    scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    wait_for_index(&mut scheduler, ids[0]);
    {
        let doc = scheduler.tab(ids[0]).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.source_generation, orig_gen);
        assert_eq!(doc.bookmarks.len(), 2);
        assert_eq!(
            doc.bookmarks.get(2).unwrap().key.record_start_offset,
            Some(offset_2)
        );
        assert_eq!(
            doc.bookmarks.get(5).unwrap().key.record_start_offset,
            Some(offset_5)
        );
    }

    // 2. Inline cell edit + Undo preserves bookmarks verbatim.
    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        let source = doc.source_cell(2, 1).unwrap().to_vec();
        doc.begin_cell_edit(2, 1, source).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "BETA_EDITED".into();
        doc.commit_cell_edit();
        assert!(doc.is_dirty());
        assert_eq!(doc.bookmarks.len(), 2);
        assert_eq!(
            doc.bookmarks.get(2).unwrap().key.record_start_offset,
            Some(offset_2)
        );
    }

    // 3. Active filter preserves bookmarks and allows toggling a visible filtered row.
    let query = FilterQuery::single(2, FilterOperator::Equals, b"a".to_vec());
    scheduler.start_filter(ids[0], query).unwrap();
    wait_for_filter(&mut scheduler, ids[0]);
    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        assert_eq!(doc.bookmarks.len(), 2);
        // Row 3 (gamma, group a) is visible under the filter; bookmark it.
        assert!(doc.toggle_bookmark_row(3).unwrap());
        assert_eq!(doc.bookmarks.len(), 3);
        assert!(
            doc.bookmarks
                .get(3)
                .unwrap()
                .key
                .record_start_offset
                .is_some()
        );
    }

    // 4. Dirty + filtered dormancy preserves all 3 bookmarks across eviction and wake.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantDirty)
    );
    scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    wait_for_index(&mut scheduler, ids[0]);
    wait_for_filter(&mut scheduler, ids[0]);
    {
        let doc = scheduler.tab(ids[0]).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.bookmarks.len(), 3);
        assert!(doc.bookmarks.contains_row(2));
        assert!(doc.bookmarks.contains_row(3));
        assert!(doc.bookmarks.contains_row(5));
    }

    // Command enablement reflects bookmark state cleanly.
    let tab = scheduler.tab(ids[0]).unwrap();
    let ctx = CommandContext {
        window: WindowId::PRIMARY,
        active_tab: tab,
        open_document_count: 2,
        write_slot_available: true,
        external_modal_blocked: false,
        text_field_focused: false,
        show_all_whitespace: false,
    };
    assert!(evaluate_command(CommandId::ToggleBookmark, &ctx).enabled);
    assert!(evaluate_command(CommandId::NextBookmark, &ctx).enabled);
    assert!(evaluate_command(CommandId::PreviousBookmark, &ctx).enabled);
    assert!(evaluate_command(CommandId::ClearBookmarks, &ctx).enabled);
}

#[test]
fn bookmarks_shift_on_insert_and_delete_rows_and_restore_on_undo() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let path = dir.path().join("rows.csv");
    fs::write(&path, b"id,val\n1,r1\n2,r2\n3,r3\n4,r4\n5,r5\n6,r6\n").unwrap();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    scheduler.register_window(WindowId::PRIMARY);
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [path.clone()], open_options(), None)
        .unwrap();
    let id = ids[0];
    wait_for_index(&mut scheduler, id);

    // Bookmark data rows 1 (r1), 3 (r3), and 5 (r5).
    let (orig_r1_off, orig_r3_off, orig_r5_off) = {
        let doc = scheduler.tab_mut(id).unwrap().document.as_mut().unwrap();
        doc.toggle_bookmark_row(1).unwrap();
        doc.toggle_bookmark_row(3).unwrap();
        doc.toggle_bookmark_row(5).unwrap();
        (
            doc.bookmarks
                .get(1)
                .unwrap()
                .key
                .record_start_offset
                .unwrap(),
            doc.bookmarks
                .get(3)
                .unwrap()
                .key
                .record_start_offset
                .unwrap(),
            doc.bookmarks
                .get(5)
                .unwrap()
                .key
                .record_start_offset
                .unwrap(),
        )
    };

    // Insert 2 blank rows before data row 3 (0-based data index 2 -> 1-based "3" -> physical position 3).
    {
        let doc = scheduler.tab_mut(id).unwrap().document.as_mut().unwrap();
        let mut dialog = StructuralDialog::insert_rows(2);
        dialog.count = "2".into();
        doc.insert_rows_from_dialog(&dialog).unwrap();
    }
    let notice = wait_for_structural(&mut scheduler, id).unwrap().unwrap();
    assert!(notice.contains("Inserted 2 rows"));
    wait_for_index(&mut scheduler, id);

    // Verify shifts: row 1 stays at 1 (r1), row 3 shifted to 5 (r3), row 5 shifted to 7 (r5),
    // and record_start_offset matches the actual rows in the new working copy!
    {
        let doc = scheduler.tab(id).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.source_generation, 2);
        let rows: Vec<u64> = doc.bookmarks.iter().map(|b| b.local_row).collect();
        assert_eq!(rows, vec![1, 5, 7]);
        let index = doc.index.as_ref().unwrap();
        for (expected_row, expected_val) in [(1, b"r1"), (5, b"r3"), (7, b"r5")] {
            let loaded = doc.session.read_rows(index, expected_row, 1).unwrap();
            assert_eq!(loaded[0].fields[1], expected_val);
            let bm = doc.bookmarks.get(expected_row).unwrap();
            assert_eq!(bm.key.source_generation, 2);
            assert_eq!(bm.key.record_start_offset, Some(loaded[0].offset));
            assert_eq!(bm.key.global_ordinal, Some(expected_row));
        }
    }

    // Delete physical rows 4..=5 (which includes bookmarked row 5 = r3!).
    {
        let doc = scheduler.tab_mut(id).unwrap().document.as_mut().unwrap();
        doc.start_delete_rows(std::iter::once(4..=5).collect())
            .unwrap();
    }
    let del_notice = wait_for_structural(&mut scheduler, id).unwrap().unwrap();
    assert!(
        del_notice.contains("1 row bookmark on deleted rows removed"),
        "expected deleted-bookmark notice, got: {del_notice}"
    );
    wait_for_index(&mut scheduler, id);

    // Row 1 stays at 1 (r1), row 5 was removed, row 7 shifted up by 2 -> row 5 (r5)!
    {
        let doc = scheduler.tab(id).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.source_generation, 3);
        let rows: Vec<u64> = doc.bookmarks.iter().map(|b| b.local_row).collect();
        assert_eq!(rows, vec![1, 5]);
        let index = doc.index.as_ref().unwrap();
        let loaded = doc.session.read_rows(index, 5, 1).unwrap();
        assert_eq!(loaded[0].fields[1], b"r5");
        assert_eq!(
            doc.bookmarks.get(5).unwrap().key.record_start_offset,
            Some(loaded[0].offset)
        );
    }

    // Undo DeleteRows restores generation 2 with all 3 bookmarks (rows 1, 5, 7)!
    scheduler.apply_undo_redo(id, false).unwrap();
    wait_for_index(&mut scheduler, id);
    {
        let doc = scheduler.tab(id).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.source_generation, 2);
        let rows: Vec<u64> = doc.bookmarks.iter().map(|b| b.local_row).collect();
        assert_eq!(rows, vec![1, 5, 7]);
        assert!(
            doc.bookmarks
                .get(5)
                .unwrap()
                .key
                .record_start_offset
                .is_some()
        );
    }

    // Redo DeleteRows returns to generation 3 (rows 1, 5).
    scheduler.apply_undo_redo(id, true).unwrap();
    wait_for_index(&mut scheduler, id);
    {
        let doc = scheduler.tab(id).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.source_generation, 3);
        let rows: Vec<u64> = doc.bookmarks.iter().map(|b| b.local_row).collect();
        assert_eq!(rows, vec![1, 5]);
    }

    // Confirm original generation offsets were distinct from post-edit generation.
    assert!(orig_r1_off < orig_r3_off && orig_r3_off < orig_r5_off);
}

#[test]
fn bookmarks_clear_with_notice_on_sort_and_dedupe_and_restore_on_undo() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let path = dir.path().join("sort_dedupe.csv");
    fs::write(&path, b"id,name\n3,charlie\n1,alpha\n2,bravo\n1,alpha\n").unwrap();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    scheduler.register_window(WindowId::PRIMARY);
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [path.clone()], open_options(), None)
        .unwrap();
    let id = ids[0];
    wait_for_index(&mut scheduler, id);

    // Bookmark rows 1 and 3.
    let (orig_off_1, orig_off_3) = {
        let doc = scheduler.tab_mut(id).unwrap().document.as_mut().unwrap();
        doc.toggle_bookmark_row(1).unwrap();
        doc.toggle_bookmark_row(3).unwrap();
        (
            doc.bookmarks
                .get(1)
                .unwrap()
                .key
                .record_start_offset
                .unwrap(),
            doc.bookmarks
                .get(3)
                .unwrap()
                .key
                .record_start_offset
                .unwrap(),
        )
    };

    // 1. Sort rows clears bookmarks with notice.
    {
        let doc = scheduler.tab_mut(id).unwrap().document.as_mut().unwrap();
        let plan = SortPlan::from(SortSpec {
            column: 0,
            mode: SortMode::Number,
            direction: SortDirection::Ascending,
            case_sensitivity: CaseSensitivity::Insensitive,
        });
        doc.start_sort_plan(plan).unwrap();
    }
    let sort_notice = wait_for_structural(&mut scheduler, id).unwrap().unwrap();
    assert!(
        sort_notice.contains("Row bookmarks were cleared because row order changed"),
        "expected sort bookmark-cleared notice, got: {sort_notice}"
    );
    wait_for_index(&mut scheduler, id);
    assert!(
        scheduler
            .tab(id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .bookmarks
            .is_empty()
    );

    // Undo Sort restores exact pre-sort bookmarks and offsets!
    scheduler.apply_undo_redo(id, false).unwrap();
    wait_for_index(&mut scheduler, id);
    {
        let doc = scheduler.tab(id).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.source_generation, 1);
        assert_eq!(doc.bookmarks.len(), 2);
        assert_eq!(
            doc.bookmarks.get(1).unwrap().key.record_start_offset,
            Some(orig_off_1)
        );
        assert_eq!(
            doc.bookmarks.get(3).unwrap().key.record_start_offset,
            Some(orig_off_3)
        );
    }

    // 2. Delete Duplicates clears bookmarks with notice and Undo restores them!
    {
        let doc = scheduler.tab_mut(id).unwrap().document.as_mut().unwrap();
        doc.start_find_duplicates_target(
            DuplicateTarget::Columns(vec![0]),
            CaseSensitivity::Insensitive,
        )
        .unwrap();
    }
    let _ = wait_for_structural(&mut scheduler, id).unwrap();
    let dedupe_notice = scheduler.confirm_duplicate_removal(id).unwrap();
    assert!(
        dedupe_notice.contains("Row bookmarks were cleared because row order changed"),
        "expected dedupe bookmark-cleared notice, got: {dedupe_notice}"
    );
    wait_for_index(&mut scheduler, id);
    assert!(
        scheduler
            .tab(id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .bookmarks
            .is_empty()
    );

    // Undo Delete Duplicates restores exact pre-dedupe bookmarks!
    scheduler.apply_undo_redo(id, false).unwrap();
    wait_for_index(&mut scheduler, id);
    {
        let doc = scheduler.tab(id).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.bookmarks.len(), 2);
        assert_eq!(
            doc.bookmarks.get(1).unwrap().key.record_start_offset,
            Some(orig_off_1)
        );
        assert_eq!(
            doc.bookmarks.get(3).unwrap().key.record_start_offset,
            Some(orig_off_3)
        );
    }
}

#[test]
fn bookmarks_preserved_across_replace_all_in_place_save_and_section_splice_save() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let path = dir.path().join("splice_bookmarks.csv");
    let mut bytes = Vec::from(&b"id,val\n"[..]);
    for i in 1..=30_u64 {
        bytes.extend_from_slice(format!("{i:02},item_{i:02}\n").as_bytes());
    }
    fs::write(&path, &bytes).unwrap();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    scheduler.register_window(WindowId::PRIMARY);

    // Open a Verified section [0, 150) of the file.
    let sec_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path.clone(),
            ByteWindow::from_range(0, 150).unwrap(),
            SectionOpenOptions {
                open: open_options(),
                ..Default::default()
            },
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open_section outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, sec_id);

    // Bookmark rows 2 and 5 in the verified section.
    {
        let doc = scheduler
            .tab_mut(sec_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        assert_eq!(doc.section_trust(), Some(BoundaryTrust::Verified));
        doc.toggle_bookmark_row(2).unwrap();
        doc.toggle_bookmark_row(5).unwrap();
    }

    // Replace All changes row lengths without changing row order -> bookmarks survive with refreshed offsets.
    {
        let doc = scheduler
            .tab_mut(sec_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.start_replace_all_with_case(b"item_", b"updated_item_", CaseSensitivity::Sensitive)
            .unwrap();
    }
    let _ = wait_for_structural(&mut scheduler, sec_id).unwrap();
    wait_for_index(&mut scheduler, sec_id);
    {
        let doc = scheduler.tab(sec_id).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.source_generation, 2);
        let index = doc.index.as_ref().unwrap();
        for row in [2_u64, 5_u64] {
            let loaded = doc.session.read_rows(index, row, 1).unwrap();
            assert!(loaded[0].fields[1].starts_with(b"updated_item_"));
            assert_eq!(
                doc.bookmarks.get(row).unwrap().key.record_start_offset,
                Some(loaded[0].offset)
            );
        }
    }

    // In-place Section Splice-Save preserves bookmarks with incremented generation and exact parent offsets!
    {
        let doc = scheduler
            .tab_mut(sec_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.start_save().unwrap();
    }
    let (_, in_place) = wait_for_save(&mut scheduler, sec_id);
    assert!(in_place);
    wait_for_index(&mut scheduler, sec_id);
    {
        let doc = scheduler.tab(sec_id).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.source_generation, 3);
        assert_eq!(doc.bookmarks.len(), 2);
        let index = doc.index.as_ref().unwrap();
        for row in [2_u64, 5_u64] {
            let loaded = doc.session.read_rows(index, row, 1).unwrap();
            assert_eq!(
                doc.bookmarks.get(row).unwrap().key.record_start_offset,
                Some(loaded[0].offset)
            );
            assert_eq!(
                doc.bookmarks.get(row).unwrap().key.global_ordinal,
                Some(row)
            );
        }
    }
}

#[test]
fn bookmarks_filter_targets_loaded_matches_and_rejects_filtered_out_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("filtered_targets.csv");
    fs::write(
        &path,
        b"id,name,group\n1,alpha,a\n2,beta,b\n3,gamma,a\n4,delta,b\n5,epsilon,a\n",
    )
    .unwrap();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        index_cache_enabled: false,
        ..Default::default()
    });
    scheduler.register_window(WindowId::PRIMARY);
    let id = scheduler
        .open_many_lazy(WindowId::PRIMARY, [path.clone()], open_options(), None)
        .unwrap()[0];
    wait_for_index(&mut scheduler, id);

    let query = FilterQuery::single(2, FilterOperator::Equals, b"a".to_vec());
    scheduler.start_filter(id, query).unwrap();
    wait_for_filter(&mut scheduler, id);

    {
        let doc = scheduler.tab_mut(id).unwrap().document.as_mut().unwrap();
        // Place selection/cursor on filtered-out row 2 (group b).
        doc.selection = Some(quarry_workbench::GridSelection::Cell { row: 2, column: 0 });
        doc.cursor.select_row(2);
        doc.selected_rows
            .select(2, quarry_workbench::SelectionModifiers::NONE);
        // Must skip filtered-out row 2 and fall back to first visible filtered row (row 1).
        assert_eq!(doc.bookmark_target_row(), Some(1));

        // Advance filter_viewport_start so visible_filter_rows() starts at match index 1 (row 3).
        doc.filter_viewport_start = 1;
        assert_eq!(doc.bookmark_target_row(), Some(3));

        // Simulate a loaded filter match whose physical row is ahead of available_rows()
        // (e.g. while structural indexing is still behind the filter scan).
        doc.index = None;
        doc.progress.rows_scanned = 2;
        doc.session = std::sync::Arc::new(
            quarry_core::Session::open(
                &path,
                OpenOptions {
                    rows: 2,
                    header_mode: HeaderMode::FirstRow,
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        assert!(doc.available_rows() <= 5);
        assert!(doc.toggle_bookmark_row(5).unwrap());
        let bm5 = doc.bookmarks.get(5).unwrap();
        assert!(bm5.key.record_start_offset.is_some());
    }
}

#[test]
fn bookmark_split_targets_rejects_cancelled_index_and_external_modification() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("split_guards.csv");
    fs::write(&path, b"id,name\n1,a\n2,b\n3,c\n4,d\n").unwrap();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        index_cache_enabled: false,
        ..Default::default()
    });
    scheduler.register_window(WindowId::PRIMARY);
    let id = scheduler
        .open_many_lazy(WindowId::PRIMARY, [path.clone()], open_options(), None)
        .unwrap()[0];
    wait_for_index(&mut scheduler, id);

    {
        let doc = scheduler.tab_mut(id).unwrap().document.as_mut().unwrap();
        doc.toggle_bookmark_row(3).unwrap();
        assert_eq!(doc.split_at_bookmarks_disabled_reason(), None);

        // A cancelled partial index must block Split-at-Bookmarks with a cancellation-specific recovery message.
        doc.progress.cancelled = true;
        let cancelled_reason = doc
            .split_at_bookmarks_disabled_reason()
            .expect("cancelled index must block Split-at-Bookmarks");
        assert!(
            cancelled_reason.contains("cancelled") && cancelled_reason.contains("Reopen"),
            "expected cancellation recovery message, got: {cancelled_reason}"
        );
        assert_eq!(doc.bookmark_split_targets().unwrap_err(), cancelled_reason);
        doc.progress.cancelled = false;
        assert_eq!(doc.split_at_bookmarks_disabled_reason(), None);

        // External file replacement must be detected before returning split targets.
        fs::write(&path, b"id,name\n10,replaced_externally\n20,other\n").unwrap();
        let err = doc.bookmark_split_targets().unwrap_err();
        assert_eq!(err, quarry_workbench::SOURCE_CHANGED_NOTICE);
        assert!(doc.source_changed);
    }
}

#[test]
fn bookmark_offsets_resolve_asynchronously_and_undo_preserves_mid_job_bookmarks() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let path = dir.path().join("async_offsets.csv");
    let mut bytes = Vec::from(&b"id,val\n"[..]);
    for i in 1..=650_u64 {
        bytes.extend_from_slice(format!("{i:04},item_{i:04}\n").as_bytes());
    }
    fs::write(&path, &bytes).unwrap();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    scheduler.register_window(WindowId::PRIMARY);
    let id = scheduler
        .open_many_lazy(WindowId::PRIMARY, [path.clone()], open_options(), None)
        .unwrap()[0];
    wait_for_index(&mut scheduler, id);

    // Bookmark row 550 (well past the initial viewport buffer of ~280 rows).
    {
        let doc = scheduler.tab_mut(id).unwrap().document.as_mut().unwrap();
        doc.toggle_bookmark_row(550).unwrap();
        assert!(
            doc.bookmarks
                .get(550)
                .unwrap()
                .key
                .record_start_offset
                .is_some()
        );
        // Replace All changes record offsets across the whole file.
        doc.start_replace_all_with_case(b"item_", b"longer_item_", CaseSensitivity::Sensitive)
            .unwrap();
    }
    let _ = wait_for_structural(&mut scheduler, id).unwrap();
    wait_for_index(&mut scheduler, id);
    {
        let doc = scheduler.tab(id).unwrap().document.as_ref().unwrap();
        let index = doc.index.as_ref().unwrap();
        let loaded = doc.session.read_rows(index, 550, 1).unwrap();
        assert_eq!(loaded[0].fields[1], b"longer_item_0550");
        assert_eq!(
            doc.bookmarks.get(550).unwrap().key.record_start_offset,
            Some(loaded[0].offset)
        );
    }

    // Start a Sort job, toggle a new bookmark (row 10) while the sort is running, then Undo Sort.
    {
        let doc = scheduler.tab_mut(id).unwrap().document.as_mut().unwrap();
        let plan = SortPlan::from(SortSpec {
            column: 0,
            mode: SortMode::Number,
            direction: SortDirection::Descending,
            case_sensitivity: CaseSensitivity::Insensitive,
        });
        doc.start_sort_plan(plan).unwrap();
        // User toggles a bookmark while the structural job is already in flight!
        doc.toggle_bookmark_row(10).unwrap();
        assert!(doc.bookmarks.contains_row(10));
    }
    let _ = wait_for_structural(&mut scheduler, id).unwrap();
    wait_for_index(&mut scheduler, id);
    assert!(
        scheduler
            .tab(id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .bookmarks
            .is_empty()
    );

    // Undo Sort must restore BOTH row 550 AND row 10 that was added while Sort ran.
    scheduler.apply_undo_redo(id, false).unwrap();
    wait_for_index(&mut scheduler, id);
    {
        let doc = scheduler.tab(id).unwrap().document.as_ref().unwrap();
        assert!(doc.bookmarks.contains_row(10));
        assert!(doc.bookmarks.contains_row(550));
    }
}

#[test]
fn dormant_clean_conflict_wake_clears_stale_bookmarks() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("conflict_one.csv");
    let p2 = dir.path().join("conflict_two.csv");
    fs::write(&p1, b"id,name\n1,alpha\n2,beta\n3,gamma\n4,delta\n").unwrap();
    fs::write(&p2, b"id,name\n1,other\n").unwrap();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    scheduler.register_window(WindowId::PRIMARY);
    let ids = scheduler
        .open_many_lazy(
            WindowId::PRIMARY,
            [p1.clone(), p2.clone()],
            open_options(),
            None,
        )
        .unwrap();
    wait_for_index(&mut scheduler, ids[0]);

    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.toggle_bookmark_row(2).unwrap();
        doc.toggle_bookmark_row(4).unwrap();
        assert_eq!(doc.bookmarks.len(), 2);
    }

    // Evict tab 0 to DormantClean, then replace its file on disk.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantClean)
    );
    fs::write(&p1, b"id,name\n100,completely_new\n200,different_rows\n").unwrap();

    // Waking tab 0 against the replacement file must clear stale bookmarks.
    scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    let doc = scheduler.tab(ids[0]).unwrap().document.as_ref().unwrap();
    assert!(doc.source_changed);
    assert!(
        doc.bookmarks.is_empty(),
        "stale bookmarks must be cleared when waking against a replaced file"
    );
}
