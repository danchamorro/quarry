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
        !doc.is_save_ready(),
        "in-place Save must be disabled for section documents in 5A-2"
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

    // Now explicitly reopen at the verified boundaries!
    let notice = scheduler.reopen_verified_section(section_id).unwrap();
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
