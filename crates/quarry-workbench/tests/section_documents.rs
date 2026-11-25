use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use quarry_core::{
    BoundaryTrust, ByteWindow, OpenOptions, SectionOpenOptions, VerifyBoundaryOutcome,
};
use quarry_workbench::{
    CommandContext, CommandId, ResidencyState, WakeOutcome, WindowId, WorkbenchScheduler,
    WorkbenchSchedulerConfig, evaluate_command,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_csv(name: &str, bytes: &[u8]) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "quarry-wb-section-{}-{}-{}.csv",
        std::process::id(),
        id,
        name
    ));
    let mut f = File::create(&path).unwrap();
    f.write_all(bytes).unwrap();
    f.flush().unwrap();
    path
}

fn wait_for_index(scheduler: &mut WorkbenchScheduler, id: quarry_workbench::DocumentId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let errs = scheduler.poll_all();
        assert!(errs.is_empty(), "unexpected indexing errors: {errs:?}");
        if scheduler
            .tab(id)
            .and_then(|t| t.document.as_ref())
            .is_some_and(|d| d.job.is_none() && d.progress.done)
        {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for index");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_verify(
    scheduler: &mut WorkbenchScheduler,
    id: quarry_workbench::DocumentId,
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
fn section_borrows_parent_index_and_coexists_with_parent_tab() {
    let mut bytes = Vec::from(&b"id,val\n"[..]);
    for i in 0..500u64 {
        bytes.extend_from_slice(format!("{i},row_{i:04}\n").as_bytes());
    }
    let path = temp_csv("parent-borrow", &bytes);
    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig::default());
    scheduler.register_window(WindowId::PRIMARY);

    let parent_id = match scheduler
        .open_path(
            WindowId::PRIMARY,
            path.clone(),
            OpenOptions::default(),
            None,
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, parent_id);

    let section_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path.clone(),
            ByteWindow::new(1500, 500).unwrap(),
            SectionOpenOptions::default(),
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected section open outcome: {other:?}"),
    };
    assert_ne!(
        parent_id, section_id,
        "section must open as its own distinct tab alongside the parent file"
    );
    wait_for_index(&mut scheduler, section_id);

    let doc = scheduler
        .tab(section_id)
        .unwrap()
        .document
        .as_ref()
        .unwrap();
    let bounds = doc.section_bounds().unwrap();
    assert_eq!(bounds.trust, BoundaryTrust::Verified);
    assert!(bounds.first_global_row().is_some());
    assert_eq!(doc.headers, vec!["id", "val"]);
    assert!(
        doc.can_splice_save_section(),
        "verified section must support in-place guarded splice-save"
    );
    assert!(
        !doc.is_save_ready(),
        "clean verified section has no edits yet"
    );
    assert!(
        doc.is_save_as_ready(),
        "Save Section As New must be enabled for section documents"
    );

    let _ = fs::remove_file(&path);
}

#[test]
fn dirty_speculative_section_preserves_drafts_on_boundary_shift_and_exports_cleanly() {
    // Build a > 20 MiB file so opening a section past 16 MiB starts as Speculative,
    // with a multiline quoted field straddling the requested start offset so Verify Boundary
    // shifts the start boundary backward!
    let mut bytes = Vec::from(&b"id,note\n"[..]);
    let pad = "p".repeat(60);
    let mut i = 0u64;
    while bytes.len() < 18 * 1024 * 1024 {
        bytes.extend_from_slice(format!("{i},{pad}\n").as_bytes());
        i += 1;
    }
    let quoted_start = bytes.len() as u64;
    bytes.extend_from_slice(b"999999,\"first part\n");
    let interior_lf_after = bytes.len() as u64;
    // > 70 KiB of CSV-looking lines inside the multiline quote so the bounded 64 KiB speculative
    // validator genuinely lands on `interior_lf_after` while Verify Boundary detects `quoted_start`.
    for k in 0..2600u64 {
        bytes.extend_from_slice(
            format!("{},inside_{k:04}_padding_for_quote_test\n", 5000 + k).as_bytes(),
        );
    }
    bytes.extend_from_slice(b"end_of_quote\"\n");
    for j in 0..30u64 {
        bytes.extend_from_slice(format!("{},after_{j}\n", 1_000_000 + j).as_bytes());
    }
    let path = temp_csv("dirty-shift", &bytes);
    let out_path = path.with_extension("draft-export.csv");
    let _ = fs::remove_file(&out_path);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig::default());
    scheduler.register_window(WindowId::PRIMARY);

    // Request inside the multiline quoted field -> speculative alignment lands on `interior_lf_after`.
    let req_start = interior_lf_after + 6;
    let req_len = 512;
    let section_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path.clone(),
            ByteWindow::new(req_start, req_len).unwrap(),
            SectionOpenOptions::default(),
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, section_id);

    // Check command availability on speculative section.
    {
        let tab = scheduler.tab(section_id).unwrap();
        let ctx = CommandContext {
            window: WindowId::PRIMARY,
            active_tab: tab,
            open_document_count: 1,
            write_slot_available: true,
            external_modal_blocked: false,
            text_field_focused: false,
            show_all_whitespace: false,
        };
        assert!(evaluate_command(CommandId::VerifySectionBoundary, &ctx).enabled);
        assert!(!evaluate_command(CommandId::Save, &ctx).enabled);
        assert!(evaluate_command(CommandId::SaveAs, &ctx).enabled);

        let doc = tab.document.as_ref().unwrap();
        assert_eq!(doc.section_trust(), Some(BoundaryTrust::Speculative));
        assert_eq!(doc.section_first_global_row(), None);
        assert!(doc.speculative_save_warning().is_some());
    }

    // Make an unsaved cell edit on a later data row inside the speculative section!
    {
        let doc = scheduler
            .tab_mut(section_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        let target_row = doc.data_start + 2;
        doc.begin_cell_edit(target_row, 1, b"after_0".to_vec())
            .unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "EDITED_DRAFT".into();
        doc.commit_cell_edit();
        assert!(doc.is_dirty());
    }

    // Run Verify Boundary and confirm pause/resume work cleanly before completion.
    scheduler.start_verify_section_boundary(section_id).unwrap();
    let completion = wait_for_verify(&mut scheduler, section_id);
    assert!(
        !completion.reopened,
        "dirty section MUST NOT auto-reopen and discard unsaved edits"
    );
    match completion.result.unwrap() {
        VerifyBoundaryOutcome::Verified {
            bounds,
            boundary_changed,
        } => {
            assert!(
                boundary_changed,
                "multiline quote must shift start boundary"
            );
            assert_eq!(bounds.snapped.base, quoted_start);
            assert_eq!(bounds.trust, BoundaryTrust::Verified);
            assert_eq!(bounds.first_global_row(), Some(i + 1));
        }
        other => panic!("expected Verified, got {other:?}"),
    }

    // Verify edits are still intact and pending_verified_reopen is set.
    {
        let doc = scheduler
            .tab(section_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap();
        assert!(doc.is_dirty(), "draft edit must be preserved");
        assert!(doc.pending_verified_reopen.is_some());
    }

    // Export the dirty draft via Save Section As New File before reopening!
    scheduler
        .tab_mut(section_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .start_save_as(out_path.clone())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let saves = scheduler.poll_saves();
        if let Some(done) = saves.into_iter().find(|s| s.document_id == section_id) {
            done.result.unwrap();
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for Save As");
        thread::sleep(Duration::from_millis(10));
    }
    let exported = fs::read_to_string(&out_path).unwrap();
    assert!(exported.starts_with("id,note\n"));
    assert!(exported.contains("EDITED_DRAFT"));

    // Unconfirmed reopen while dirty must be refused so unsaved drafts cannot be silently lost.
    assert!(
        scheduler
            .reopen_verified_section(section_id, false)
            .is_err(),
        "reopen_verified_section must refuse while dirty unless discard_unsaved is confirmed"
    );

    // Now explicitly reopen at the verified boundaries with confirmed discard!
    let notice = scheduler.reopen_verified_section(section_id, true).unwrap();
    assert!(notice.contains("verified"));
    wait_for_index(&mut scheduler, section_id);
    let doc = scheduler
        .tab(section_id)
        .unwrap()
        .document
        .as_ref()
        .unwrap();
    assert_eq!(doc.section_trust(), Some(BoundaryTrust::Verified));
    assert_eq!(doc.section_first_global_row(), Some(i + 1));
    assert_eq!(doc.section_bounds().unwrap().snapped.base, quoted_start);
    assert!(!doc.is_dirty());

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&out_path);
}

#[test]
fn clean_section_survives_dormancy_round_trip() {
    let mut bytes = Vec::from(&b"k,v\n"[..]);
    for i in 0..400u64 {
        bytes.extend_from_slice(format!("{i},val_{i:04}\n").as_bytes());
    }
    let path1 = temp_csv("dormant-sec-1", &bytes);
    let path2 = temp_csv("dormant-sec-2", &bytes);
    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        ..WorkbenchSchedulerConfig::default()
    });
    scheduler.register_window(WindowId::PRIMARY);

    let sec_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path1.clone(),
            ByteWindow::new(500, 400).unwrap(),
            SectionOpenOptions::default(),
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, sec_id);
    let orig_bounds = scheduler
        .tab(sec_id)
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .section_bounds()
        .unwrap();

    // Open a second file with max_hydrated_tabs = 1 -> evicts the clean section to dormancy.
    let _ = scheduler
        .open_path(
            WindowId::PRIMARY,
            path2.clone(),
            OpenOptions::default(),
            None,
        )
        .unwrap();
    assert_eq!(
        scheduler.residency(sec_id),
        Some(ResidencyState::DormantClean)
    );

    // Wake the section tab back up and verify section bounds & headers survived cleanly.
    let _ = scheduler.activate_tab(WindowId::PRIMARY, sec_id).unwrap();
    wait_for_index(&mut scheduler, sec_id);
    let woken = scheduler.tab(sec_id).unwrap().document.as_ref().unwrap();
    assert!(woken.is_section());
    assert_eq!(woken.section_bounds().unwrap().snapped, orig_bounds.snapped);
    assert_eq!(woken.headers, vec!["k", "v"]);

    let _ = fs::remove_file(&path1);
    let _ = fs::remove_file(&path2);
}

#[test]
fn verified_section_preserves_proof_across_dormancy_reloads_bounded_and_blocks_join() {
    // Build a > 16 MiB file with a large multiline quoted record so parent-indexed Verified
    // opening snaps to `quoted_start` whereas naive speculative alignment would snap inside.
    let mut bytes = Vec::from(&b"id,note\n"[..]);
    let pad = "a".repeat(60);
    let mut i = 0u64;
    while bytes.len() < 16 * 1024 * 1024 + 128 * 1024 {
        bytes.extend_from_slice(format!("{i},{pad}\n").as_bytes());
        i += 1;
    }
    let quoted_start = bytes.len() as u64;
    bytes.extend_from_slice(format!("{i},\"open_line\n").as_bytes());
    for k in 0..2400u64 {
        bytes.extend_from_slice(format!("inner_{k:04},text_payload_line\n").as_bytes());
    }
    let interior = bytes.len() as u64 - 160;
    bytes.extend_from_slice(b"closing_line\"\n");
    let quoted_end = bytes.len() as u64;
    bytes.extend_from_slice(b"999999,after\n");

    let path1 = temp_csv("sec-verified-dormant", &bytes);
    let path2 = temp_csv("sec-other-evictor", b"a,b\n1,2\n");
    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 2,
        ..WorkbenchSchedulerConfig::default()
    });
    scheduler.register_window(WindowId::PRIMARY);

    // 1. Open parent and wait for its full StructuralIndex, then open a section inside the
    // multiline record so it borrows the parent index and becomes Verified at `[quoted_start, quoted_end)`.
    let parent_id = match scheduler
        .open_path(
            WindowId::PRIMARY,
            path1.clone(),
            OpenOptions::default(),
            None,
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected parent outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, parent_id);

    let sec_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path1.clone(),
            ByteWindow::new(interior, 64).unwrap(),
            SectionOpenOptions::default(),
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected section outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, sec_id);

    // Verify section tab is excluded from open-tab Join choices and rejected by capture_join_document.
    let join_ids: Vec<_> = scheduler
        .hydrated_join_choices()
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    assert!(join_ids.contains(&parent_id));
    assert!(!join_ids.contains(&sec_id));
    assert!(
        scheduler
            .tab_mut(sec_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap()
            .capture_join_document(sec_id.raw())
            .is_err()
    );

    let orig_bounds = scheduler
        .tab(sec_id)
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .section_bounds()
        .unwrap();
    assert_eq!(orig_bounds.trust, BoundaryTrust::Verified);
    assert_eq!(orig_bounds.snapped.base, quoted_start);
    assert_eq!(orig_bounds.snapped_end(), quoted_end);

    // 2. Close parent, open another file to evict the section (max_hydrated_tabs = 1 via new tab),
    // wait: with max_hydrated_tabs = 2, close parent and open 2 files to evict sec_id.
    scheduler.close_tab(parent_id).unwrap();
    let path3 = temp_csv("sec-other-evictor-2", b"c,d\n3,4\n");
    let _ = scheduler
        .open_path(
            WindowId::PRIMARY,
            path2.clone(),
            OpenOptions::default(),
            None,
        )
        .unwrap();
    let _ = scheduler
        .open_path(
            WindowId::PRIMARY,
            path3.clone(),
            OpenOptions::default(),
            None,
        )
        .unwrap();
    assert_eq!(
        scheduler.residency(sec_id),
        Some(ResidencyState::DormantClean)
    );

    // 3. Wake the section back up without any parent tab open: must preserve Verified trust,
    // exact snapped range `[quoted_start, quoted_end)`, and global row number!
    let _ = scheduler.activate_tab(WindowId::PRIMARY, sec_id).unwrap();
    wait_for_index(&mut scheduler, sec_id);
    let woken = scheduler.tab(sec_id).unwrap().document.as_ref().unwrap();
    let woken_bounds = woken.section_bounds().unwrap();
    assert_eq!(woken_bounds.trust, BoundaryTrust::Verified);
    assert_eq!(woken_bounds.snapped, orig_bounds.snapped);
    assert_eq!(
        woken_bounds.first_global_row(),
        orig_bounds.first_global_row()
    );

    // 4. Reload / reopen_tab on the section must keep it as a section, NEVER opening the whole file.
    let _ = scheduler
        .reopen_tab(WindowId::PRIMARY, sec_id, OpenOptions::default(), None)
        .unwrap();
    wait_for_index(&mut scheduler, sec_id);
    let reloaded = scheduler.tab(sec_id).unwrap().document.as_ref().unwrap();
    assert!(
        reloaded.is_section(),
        "reopen_tab on a section must keep it bounded as a section"
    );

    let _ = fs::remove_file(&path1);
    let _ = fs::remove_file(&path2);
    let _ = fs::remove_file(&path3);
}

#[test]
fn dirty_section_preserves_verified_override_across_dormancy_and_respects_blocked_auto_reopen() {
    let mut bytes = Vec::from(&b"id,note\n"[..]);
    let pad = "p".repeat(60);
    let mut i = 0u64;
    while bytes.len() < 18 * 1024 * 1024 {
        bytes.extend_from_slice(format!("{i},{pad}\n").as_bytes());
        i += 1;
    }
    let quoted_start = bytes.len() as u64;
    bytes.extend_from_slice(b"999999,\"first part\n");
    let interior_lf_after = bytes.len() as u64;
    for k in 0..2600u64 {
        bytes.extend_from_slice(
            format!("{},inside_{k:04}_padding_for_quote_test\n", 5000 + k).as_bytes(),
        );
    }
    bytes.extend_from_slice(b"end_of_quote\"\n");
    for j in 0..20u64 {
        bytes.extend_from_slice(format!("{},after_{j}\n", 1_000_000 + j).as_bytes());
    }
    let path1 = temp_csv("dirty-verify-dormant", &bytes);
    let path2 = temp_csv("dirty-verify-evictor", b"x,y\n1,2\n");

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        ..WorkbenchSchedulerConfig::default()
    });
    scheduler.register_window(WindowId::PRIMARY);

    // 1. Open speculative section and defer auto-reopen via `blocked_auto_reopen` (simulating an
    // active cell editor with an unapplied draft). Even though the document isn't dirty yet,
    // verification must NOT silently replace the session while blocked.
    let sec_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path1.clone(),
            ByteWindow::new(interior_lf_after + 6, 512).unwrap(),
            SectionOpenOptions::default(),
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, sec_id);
    scheduler.start_verify_section_boundary(sec_id).unwrap();

    let blocked = std::collections::BTreeSet::from([sec_id]);
    let deadline = Instant::now() + Duration::from_secs(15);
    let completion = loop {
        if let Some(c) = scheduler
            .poll_section_verifications_with_blocked(&blocked)
            .into_iter()
            .find(|c| c.document_id == sec_id)
        {
            break c;
        }
        assert!(Instant::now() < deadline, "timed out waiting for verify");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        !completion.reopened,
        "blocked clean section must not auto-reopen while an editor is active"
    );
    assert_eq!(
        scheduler
            .tab(sec_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .pending_verified_reopen
            .map(|b| b.snapped.base),
        Some(quoted_start)
    );

    // 2. Now reopen verified, make a cell edit, verify an in-place verified override survives
    // dormancy round-trip when evicted by another tab!
    let _ = scheduler.reopen_verified_section(sec_id, false).unwrap();
    wait_for_index(&mut scheduler, sec_id);
    let verified_bounds = scheduler
        .tab(sec_id)
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .section_bounds()
        .unwrap();
    assert_eq!(verified_bounds.trust, BoundaryTrust::Verified);
    assert_eq!(verified_bounds.snapped.base, quoted_start);
    {
        let doc = scheduler
            .tab_mut(sec_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.verified_bounds_override = Some(verified_bounds);
        let target_row = doc.data_start;
        doc.begin_cell_edit(target_row, 0, b"999999".to_vec())
            .unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "DIRTY_KEEP".into();
        doc.commit_cell_edit();
    }

    let _ = scheduler
        .open_path(
            WindowId::PRIMARY,
            path2.clone(),
            OpenOptions::default(),
            None,
        )
        .unwrap();
    assert_eq!(
        scheduler.residency(sec_id),
        Some(ResidencyState::DormantDirty)
    );
    assert!(
        scheduler.tab_display_name(sec_id).unwrap().contains('['),
        "dormant dirty section tab display name should keep byte range suffix"
    );

    let _ = scheduler.activate_tab(WindowId::PRIMARY, sec_id).unwrap();
    wait_for_index(&mut scheduler, sec_id);
    let woken = scheduler.tab(sec_id).unwrap().document.as_ref().unwrap();
    assert!(woken.is_dirty());
    assert_eq!(woken.verified_bounds_override, Some(verified_bounds));
    assert_eq!(woken.section_trust(), Some(BoundaryTrust::Verified));

    let _ = fs::remove_file(&path1);
    let _ = fs::remove_file(&path2);
}

#[test]
fn interior_section_header_rename_uses_captured_header_and_reordered_columns_reject_save() {
    let raw = b"name,value\nalice,1\nbob,2\n";
    let path = temp_csv("header-rename-and-reorder", raw);
    let out_path = path.with_extension("reordered-out.csv");
    let _ = fs::remove_file(&out_path);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig::default());
    scheduler.register_window(WindowId::PRIMARY);

    let sec_start = b"name,value\n".len() as u64;
    let sec_end = sec_start + b"alice,1\n".len() as u64;
    let sec_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path.clone(),
            ByteWindow::from_range(sec_start, sec_end).unwrap(),
            SectionOpenOptions::default(),
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, sec_id);

    // Verify boundaries so we specifically exercise the reordered-column gate on a Verified section.
    scheduler.start_verify_section_boundary(sec_id).unwrap();
    let _ = wait_for_verify(&mut scheduler, sec_id);

    let doc = scheduler
        .tab_mut(sec_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap();
    assert_eq!(doc.section_trust(), Some(BoundaryTrust::Verified));
    // 1. Source header name for an interior section comes from byte-0 header ("name"),
    // NOT the section's first data row ("alice"), and header editing is disabled on sections.
    assert_eq!(doc.source_header_name(0), Some("name"));
    assert!(!doc.header_is_editable(0));
    assert!(doc.rename_header(0, "alice".into()).is_err());

    // Edit a data cell instead so the section is dirty.
    doc.begin_cell_edit(0, 0, b"alice".to_vec()).unwrap();
    doc.cell_edit.as_mut().unwrap().draft = "ALICE".into();
    doc.commit_cell_edit();
    assert!(doc.is_dirty());

    // 2. Reordering columns on a section must reject BOTH in-place splice-save and Save As
    // rather than silently dropping column order.
    doc.move_column(1, 0).unwrap();
    assert!(!doc.is_save_ready());
    let save_err = doc.start_save().unwrap_err();
    assert!(
        save_err.text.to_lowercase().contains("column layout"),
        "expected column layout reset error, got: {}",
        save_err.text
    );
    let err = doc.start_save_as(out_path.clone()).unwrap_err();
    assert!(
        err.text.to_lowercase().contains("reordered") && err.text.contains("Columns panel"),
        "expected actionable column-layout reset error, got: {}",
        err.text
    );
    assert!(!out_path.exists());

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&out_path);
}

#[test]
fn dormant_section_reopen_resets_row_coordinates_when_header_mode_changes() {
    let raw = b"h1,h2\nr1,v1\nr2,v2\n";
    let path1 = temp_csv("dormant-reopen-hdr", raw);
    let path2 = temp_csv("dormant-reopen-evictor", b"x,y\n1,2\n");
    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        ..WorkbenchSchedulerConfig::default()
    });
    scheduler.register_window(WindowId::PRIMARY);

    let sec_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path1.clone(),
            ByteWindow::from_range(0, 12).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: quarry_core::HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, sec_id);
    assert_eq!(
        scheduler
            .tab(sec_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .data_start,
        1
    );

    // Evict section to dormancy, then reopen with NoHeader.
    let _ = scheduler
        .open_path(
            WindowId::PRIMARY,
            path2.clone(),
            OpenOptions::default(),
            None,
        )
        .unwrap();
    assert_eq!(
        scheduler.residency(sec_id),
        Some(ResidencyState::DormantClean)
    );

    let _ = scheduler
        .reopen_tab(
            WindowId::PRIMARY,
            sec_id,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: quarry_core::HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
            None,
        )
        .unwrap();
    wait_for_index(&mut scheduler, sec_id);
    let doc = scheduler.tab(sec_id).unwrap().document.as_ref().unwrap();
    assert!(doc.is_section());
    assert_eq!(
        doc.data_start, 0,
        "reopening dormant byte-0 section as NoHeader must reset data_start to 0"
    );

    let _ = fs::remove_file(&path1);
    let _ = fs::remove_file(&path2);
}

#[test]
fn dormant_section_reopen_tab_resets_view_when_delimiter_changes() {
    let path1 = temp_csv("dormant-reopen-delim", b"a;b\n1;2\n");
    let path2 = temp_csv("dormant-reopen-delim-evictor", b"x,y\n1,2\n");
    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        ..WorkbenchSchedulerConfig::default()
    });
    scheduler.register_window(WindowId::PRIMARY);

    let sec_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path1.clone(),
            ByteWindow::from_range(0, 8).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b';'),
                    header_mode: quarry_core::HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, sec_id);
    {
        let doc = scheduler.tab(sec_id).unwrap().document.as_ref().unwrap();
        assert_eq!(doc.total_columns, 2);
        assert_eq!(doc.headers, vec!["a".to_string(), "b".to_string()]);
    }

    // Evict section to dormancy, then reopen with comma delimiter (same header mode,
    // encoding, data_start, and snapped bounds).
    let _ = scheduler
        .open_path(
            WindowId::PRIMARY,
            path2.clone(),
            OpenOptions::default(),
            None,
        )
        .unwrap();
    assert_eq!(
        scheduler.residency(sec_id),
        Some(ResidencyState::DormantClean)
    );

    let _ = scheduler
        .reopen_tab(
            WindowId::PRIMARY,
            sec_id,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: quarry_core::HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
            None,
        )
        .unwrap();
    wait_for_index(&mut scheduler, sec_id);
    let doc = scheduler.tab(sec_id).unwrap().document.as_ref().unwrap();
    assert!(doc.is_section());
    assert_eq!(
        doc.total_columns, 1,
        "reopening a dormant section with a different delimiter must reset stale column count"
    );
    assert_eq!(
        doc.headers,
        vec!["a;b".to_string()],
        "reopening a dormant section with a different delimiter must reset stale headers"
    );

    let _ = fs::remove_file(&path1);
    let _ = fs::remove_file(&path2);
}

#[test]
fn verified_section_row_edits_replace_all_undo_splice_save_and_sibling_coordination() {
    let prefix = b"id,tag,note\n0,old,head\n";
    let middle = b"1,old,alpha\n2,keep,beta\n3,old,gamma\n";
    let suffix = b"4,old,tail\n";
    let mut initial = Vec::new();
    initial.extend_from_slice(prefix);
    initial.extend_from_slice(middle);
    initial.extend_from_slice(suffix);
    let path = temp_csv("splice-full-flow", &initial);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 4,
        ..WorkbenchSchedulerConfig::default()
    });
    scheduler.register_window(WindowId::PRIMARY);

    // 1. Open parent file and wait for index.
    let parent_id = match scheduler
        .open_path(
            WindowId::PRIMARY,
            path.clone(),
            OpenOptions::default(),
            None,
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, parent_id);

    // 2. Open verified interior section covering rows 1..=3.
    let sec_start = prefix.len() as u64;
    let sec_len = middle.len() as u64;
    let sec_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path.clone(),
            ByteWindow::new(sec_start, sec_len).unwrap(),
            SectionOpenOptions::default(),
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected section open outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, sec_id);

    // Verify Insert Rows command is enabled on Verified section while Insert Columns is disabled!
    {
        let tab = scheduler.tab(sec_id).unwrap();
        let ctx = CommandContext {
            window: WindowId::PRIMARY,
            active_tab: tab,
            open_document_count: 2,
            write_slot_available: true,
            external_modal_blocked: false,
            text_field_focused: false,
            show_all_whitespace: false,
        };
        assert!(evaluate_command(CommandId::InsertRows, &ctx).enabled);
        assert!(!evaluate_command(CommandId::InsertColumns, &ctx).enabled);
        assert!(!evaluate_command(CommandId::Save, &ctx).enabled);
    }

    // 3. Edit cell (0, 2) -> "ALPHA", confirm dirty mutual exclusion blocks parent tab!
    {
        let doc = scheduler
            .tab_mut(sec_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.begin_cell_edit(0, 2, b"alpha".to_vec()).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "ALPHA".into();
        doc.commit_cell_edit();
        assert!(doc.is_dirty());
        assert!(doc.is_save_ready());
    }
    assert!(
        scheduler.sibling_dirty_conflict(parent_id).is_some(),
        "dirty section must block editing the parent tab for the same file"
    );
    assert!(
        scheduler.sibling_dirty_conflict(sec_id).is_none(),
        "dirty section must not conflict with itself"
    );

    // 4. Insert 1 blank row at local position 1 on the Verified section.
    let dialog = quarry_workbench::StructuralDialog::insert_rows(1);
    scheduler
        .tab_mut(sec_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .insert_rows_from_dialog(&dialog)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let done = scheduler.poll_structural_edits();
        if let Some(comp) = done.into_iter().find(|c| c.document_id == sec_id) {
            comp.result.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for insert rows"
        );
        thread::sleep(Duration::from_millis(10));
    }
    wait_for_index(&mut scheduler, sec_id);
    assert_eq!(
        scheduler
            .tab(sec_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .available_data_rows(),
        4
    );

    // 5. Undo then Redo the section structural edit!
    scheduler.apply_undo_redo(sec_id, false).unwrap();
    wait_for_index(&mut scheduler, sec_id);
    assert_eq!(
        scheduler
            .tab(sec_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .available_data_rows(),
        3
    );
    scheduler.apply_undo_redo(sec_id, true).unwrap();
    wait_for_index(&mut scheduler, sec_id);
    assert_eq!(
        scheduler
            .tab(sec_id)
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .available_data_rows(),
        4
    );

    // 6. Replace All "old" -> "NEW" inside the section working copy (must NOT touch parent prefix/suffix).
    scheduler
        .tab_mut(sec_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .start_replace_all_with_case(b"old", b"NEW", quarry_core::CaseSensitivity::Sensitive)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let done = scheduler.poll_structural_edits();
        if let Some(comp) = done.into_iter().find(|c| c.document_id == sec_id) {
            comp.result.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for replace all"
        );
        thread::sleep(Duration::from_millis(10));
    }
    wait_for_index(&mut scheduler, sec_id);

    // 7. Delete local row 2 ("2,keep,beta").
    scheduler
        .tab_mut(sec_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .start_delete_rows(std::iter::once(2..=2).collect())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let done = scheduler.poll_structural_edits();
        if let Some(comp) = done.into_iter().find(|c| c.document_id == sec_id) {
            comp.result.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for delete rows"
        );
        thread::sleep(Duration::from_millis(10));
    }
    wait_for_index(&mut scheduler, sec_id);

    // 8. Splice-save back into the parent file!
    scheduler
        .tab_mut(sec_id)
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .start_save()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let saves = scheduler.poll_saves();
        if let Some(comp) = saves.into_iter().find(|s| s.document_id == sec_id) {
            let (dest, in_place) = comp.result.unwrap();
            assert_eq!(dest, path);
            assert!(in_place);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for splice save"
        );
        thread::sleep(Duration::from_millis(10));
    }
    wait_for_index(&mut scheduler, sec_id);

    // Verify parent file bytes: untouched prefix & suffix, updated section!
    assert_eq!(
        fs::read(&path).unwrap(),
        b"id,tag,note\n0,old,head\n1,NEW,ALPHA\n,,\n3,NEW,gamma\n4,old,tail\n"
    );

    // Verify section reloaded cleanly at updated Verified bounds and is now clean!
    let sec_doc = scheduler.tab(sec_id).unwrap().document.as_ref().unwrap();
    assert!(sec_doc.is_section());
    assert!(!sec_doc.is_dirty());
    assert_eq!(sec_doc.section_trust(), Some(BoundaryTrust::Verified));
    assert_eq!(sec_doc.section_bounds().unwrap().snapped.base, sec_start);
    assert_eq!(
        sec_doc.section_bounds().unwrap().snapped.len,
        b"1,NEW,ALPHA\n,,\n3,NEW,gamma\n".len() as u64
    );
    assert_eq!(sec_doc.available_data_rows(), 3);

    // Verify open clean parent tab was immediately marked `source_changed = true`!
    let parent_tab = scheduler.tab(parent_id).unwrap();
    assert!(
        parent_tab.document.as_ref().unwrap().source_changed,
        "parent tab must be immediately marked source_changed after section splice-save"
    );
    assert!(parent_tab.notice.is_some());

    let _ = fs::remove_file(&path);
}

#[test]
fn speculative_section_blocks_row_edits_and_splice_save_until_verified_in_place() {
    let mut bytes = Vec::from(&b"id,val\n"[..]);
    let pad = "p".repeat(60);
    let mut i = 0u64;
    while bytes.len() < 17 * 1024 * 1024 {
        bytes.extend_from_slice(format!("{i},{pad}\n").as_bytes());
        i += 1;
    }
    let sec_start = bytes.len() as u64;
    bytes.extend_from_slice(b"900001,target_one\n900002,target_two\n");
    let sec_len = b"900001,target_one\n900002,target_two\n".len() as u64;
    bytes.extend_from_slice(b"900003,tail_untouched\n");
    let path = temp_csv("spec-then-verify-splice", &bytes);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig::default());
    scheduler.register_window(WindowId::PRIMARY);

    let sec_id = match scheduler
        .open_section(
            WindowId::PRIMARY,
            path.clone(),
            ByteWindow::new(sec_start, sec_len).unwrap(),
            SectionOpenOptions::default(),
        )
        .unwrap()
    {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("unexpected open outcome: {other:?}"),
    };
    wait_for_index(&mut scheduler, sec_id);

    // Speculative section blocks row insert, row delete, replace all, and in-place Save.
    {
        let doc = scheduler
            .tab_mut(sec_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        assert_eq!(doc.section_trust(), Some(BoundaryTrust::Speculative));
        assert!(doc.row_structural_edit_disabled_reason().is_some());
        assert!(!doc.can_splice_save_section());
        doc.begin_cell_edit(0, 1, b"target_one".to_vec()).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "EDITED_ONE".into();
        doc.commit_cell_edit();
        assert!(!doc.is_save_ready());
        assert!(doc.start_save().is_err());
    }

    // Run Verify Boundary -> same boundaries (boundary_changed == false) upgrades Session in-place!
    scheduler.start_verify_section_boundary(sec_id).unwrap();
    let comp = wait_for_verify(&mut scheduler, sec_id);
    assert!(!comp.reopened);
    assert!(matches!(
        comp.result.unwrap(),
        VerifyBoundaryOutcome::Verified {
            boundary_changed: false,
            ..
        }
    ));

    // Now the already-dirty section is Verified and CAN splice-save directly back to parent!
    {
        let doc = scheduler
            .tab_mut(sec_id)
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        assert_eq!(doc.section_trust(), Some(BoundaryTrust::Verified));
        assert!(doc.is_save_ready());
        doc.start_save().unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let saves = scheduler.poll_saves();
        if let Some(c) = saves.into_iter().find(|s| s.document_id == sec_id) {
            c.result.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for splice save"
        );
        thread::sleep(Duration::from_millis(10));
    }
    wait_for_index(&mut scheduler, sec_id);

    let updated = fs::read(&path).unwrap();
    assert_eq!(
        &updated[sec_start as usize..(sec_start + sec_len) as usize],
        b"900001,EDITED_ONE\n900002,target_two\n"
    );
    assert!(updated.ends_with(b"900003,tail_untouched\n"));

    let _ = fs::remove_file(&path);
}
