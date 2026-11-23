//! Headless verification suite for Phase 4A-2 dormant-tab scheduler, 200-file open,
//! multi-window hydration guarantees, code-counted `< 128` FD budget, dirty/filter/`.gz`
//! dormancy policies, external source-change conflicts, and quit cleanup vs cancellation.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use quarry_core::{
    CaseSensitivity, ColumnTransformation, DuplicateTarget, FilterOperator, FilterQuery,
    HeaderMode, OpenOptions,
};
use quarry_workbench::{
    CompressedDormancyPolicy, DocumentId, FdCategory, GridSelection, MAX_PROCESS_FD_BUDGET,
    PinReason, ResidencyState, SOURCE_CHANGED_NOTICE, SchedulerError, StructuralJob, WakeKind,
    WakeOutcome, WindowId, WorkbenchScheduler, WorkbenchSchedulerConfig, WorkingCopyState,
    compressed,
};

fn write_csv(dir: &Path, name: &str, rows: usize) -> PathBuf {
    let path = dir.join(name);
    let mut out = String::from("id,category,amount,notes\n");
    for row in 1..=rows {
        let cat = match row % 4 {
            0 => "alpha",
            1 => "beta",
            2 => "gamma",
            _ => "delta",
        };
        out.push_str(&format!("{row},{cat},{},payload_{row}\n", row * 13));
    }
    fs::write(&path, out).unwrap();
    path
}

fn write_gzip_csv(dir: &Path, name: &str, rows: usize) -> PathBuf {
    let path = dir.path_or_self().join(name);
    let mut raw = String::from("id,category,amount\n");
    for row in 1..=rows {
        raw.push_str(&format!("{row},cat{},{}\n", row % 3, row * 7));
    }
    let file = fs::File::create(&path).unwrap();
    let mut encoder = GzEncoder::new(file, Compression::fast());
    encoder.write_all(raw.as_bytes()).unwrap();
    encoder.finish().unwrap();
    path
}

trait PathHelper {
    fn path_or_self(&self) -> &Path;
}
impl PathHelper for Path {
    fn path_or_self(&self) -> &Path {
        self
    }
}

fn finish_active_indexing(scheduler: &mut WorkbenchScheduler, id: DocumentId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let errors = scheduler.poll_all();
        assert!(errors.is_empty(), "unexpected poll errors: {errors:?}");
        let done = scheduler
            .tab(id)
            .and_then(|tab| tab.document.as_ref())
            .is_some_and(|doc| doc.job.is_none() && !doc.decompressing());
        if done {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "indexing/decompression timed out"
        );
        std::thread::yield_now();
    }
}

fn finish_active_filter(scheduler: &mut WorkbenchScheduler, id: DocumentId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let errors = scheduler.poll_all();
        assert!(
            errors.is_empty(),
            "unexpected filter poll errors: {errors:?}"
        );
        let done = scheduler
            .tab(id)
            .and_then(|tab| tab.document.as_ref())
            .is_some_and(|doc| doc.filter_job.is_none() && !doc.filter_rows_loading());
        if done {
            break;
        }
        assert!(Instant::now() < deadline, "filter timed out");
        std::thread::yield_now();
    }
}

fn open_options() -> OpenOptions {
    OpenOptions {
        header_mode: HeaderMode::FirstRow,
        ..Default::default()
    }
}

#[cfg(unix)]
fn count_process_open_fds() -> Option<usize> {
    for dir in ["/dev/fd", "/proc/self/fd"] {
        if let Ok(entries) = fs::read_dir(dir) {
            return Some(entries.count());
        }
    }
    None
}

#[test]
fn opening_200_files_lazily_keeps_fd_budget_under_128_and_evicts_clean_tabs() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = (0..200)
        .map(|i| write_csv(dir.path(), &format!("doc_{i:03}.csv"), 120))
        .collect();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 8,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });

    // Reserve non-document FD categories (GUI/system, sidecar, temp, cleanup) to verify
    // total process FD reservations stay strictly below 128 across 200 open files.
    let _gui_fds = scheduler
        .fd_budget()
        .try_reserve(FdCategory::GuiSystem, 12)
        .unwrap();
    let _sidecar_fds = scheduler
        .fd_budget()
        .try_reserve(FdCategory::SidecarCache, 4)
        .unwrap();

    let win1 = WindowId::new(1);
    let win2 = WindowId::new(2);
    let win3 = WindowId::new(3);
    let win4 = WindowId::new(4);

    let ids = scheduler
        .open_many_lazy(win1, paths.clone(), open_options(), None)
        .unwrap();
    assert_eq!(ids.len(), 200);
    assert_eq!(scheduler.total_tabs(), 200);
    assert_eq!(scheduler.hydrated_count(), 1);
    assert_eq!(scheduler.stub_count(), 199);
    finish_active_indexing(&mut scheduler, ids[0]);

    // Hydrate tabs across 4 windows and cycle through 24 tabs to exercise eviction + cold wake.
    for (win, idx) in [(win2, 50), (win3, 100), (win4, 150)] {
        let outcome = scheduler.activate_tab(win, ids[idx]).unwrap();
        assert!(matches!(
            outcome,
            WakeOutcome::Ready {
                kind: WakeKind::StubInitialOpen,
                ..
            }
        ));
        finish_active_indexing(&mut scheduler, ids[idx]);
    }
    assert_eq!(scheduler.hydrated_count(), 4);

    for &idx in &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] {
        let outcome = scheduler.activate_tab(win1, ids[idx]).unwrap();
        assert!(matches!(outcome, WakeOutcome::Ready { .. }));
        finish_active_indexing(&mut scheduler, ids[idx]);
        assert!(scheduler.hydrated_count() <= 8);
    }
    // Earlier clean tabs were evicted to DormantClean while each of the 4 windows kept >= 1 hydrated tab.
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantClean)
    );
    for (win, idx) in [(win2, 50), (win3, 100), (win4, 150)] {
        assert_eq!(scheduler.active_document(win), Some(ids[idx]));
        assert_eq!(
            scheduler.residency(ids[idx]),
            Some(ResidencyState::Hydrated)
        );
    }

    // Wake a clean dormant tab (cold wake) and verify it restores cleanly.
    let wake_cold = scheduler.activate_tab(win1, ids[0]).unwrap();
    assert!(matches!(
        wake_cold,
        WakeOutcome::Ready {
            kind: WakeKind::CleanCold,
            ..
        }
    ));
    finish_active_indexing(&mut scheduler, ids[0]);
    assert!(
        !scheduler
            .tab(ids[0])
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .buffered_rows
            .is_empty()
    );

    // Re-activating an already hydrated tab is a warm hit.
    let wake_warm = scheduler.activate_tab(win2, ids[50]).unwrap();
    assert!(matches!(
        wake_warm,
        WakeOutcome::Ready {
            kind: WakeKind::AlreadyHydrated,
            ..
        }
    ));

    // Verify code-counted FD ledger AND actual OS open FD count stay strictly below 128.
    let fd_snap = scheduler.fd_budget().snapshot();
    assert!(
        fd_snap.peak_reserved < MAX_PROCESS_FD_BUDGET,
        "code-counted peak FDs {} must be < 128",
        fd_snap.peak_reserved
    );
    assert!(fd_snap.total_reserved < MAX_PROCESS_FD_BUDGET);
    #[cfg(unix)]
    if let Some(os_fds) = count_process_open_fds() {
        assert!(
            os_fds < MAX_PROCESS_FD_BUDGET,
            "actual process open FDs ({os_fds}) must stay < 128 across 200 open files"
        );
    }

    // Completed quit cleans up cleanly with zero errors.
    let errors = scheduler.finalize_quit();
    assert!(errors.is_empty());
    assert_eq!(scheduler.total_tabs(), 0);
}

#[test]
fn dormant_and_stub_tabs_deduplicate_hard_links_and_reserve_working_paths() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "a.csv", 20);
    let b = write_csv(dir.path(), "b.csv", 20);
    let c = write_csv(dir.path(), "c.csv", 20);
    let a_link = dir.path().join("a-hardlink.csv");
    fs::hard_link(&a, &a_link).unwrap();
    let c_link = dir.path().join("c-hardlink.csv");
    fs::hard_link(&c, &c_link).unwrap();

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(
            WindowId::PRIMARY,
            [a.clone(), b.clone(), c.clone()],
            open_options(),
            None,
        )
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    // Switch to `b`, evicting `a` to DormantClean while `c` remains a lazy Stub.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantClean)
    );
    assert_eq!(scheduler.residency(ids[2]), Some(ResidencyState::Stub));

    // Hard-link lookup resolves both dormant `a` and stub `c` without duplicating tabs.
    assert_eq!(
        scheduler.find_owner_for_path(&a_link),
        Some((WindowId::PRIMARY, ids[0]))
    );
    assert_eq!(
        scheduler.find_owner_for_path(&c_link),
        Some((WindowId::PRIMARY, ids[2]))
    );
    assert!(scheduler.path_reserved(&a_link));
    assert!(scheduler.path_reserved(&c_link));

    // Opening the hard link activates the existing dormant tab rather than creating a 4th tab.
    let outcome = scheduler
        .open_path(WindowId::PRIMARY, a_link, open_options(), None)
        .unwrap();
    assert!(matches!(
        outcome,
        WakeOutcome::Ready {
            document_id,
            kind: WakeKind::CleanCold,
            ..
        } if document_id == ids[0]
    ));
    assert_eq!(scheduler.total_tabs(), 3);
}

#[test]
fn dirty_tab_dormancy_preserves_edits_and_working_copies_and_enforces_overlay_and_disk_caps() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "dirty-a.csv", 20);
    let b = write_csv(dir.path(), "clean-b.csv", 20);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        max_dirty_overlay_bytes: 64 * 1024,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(
            WindowId::PRIMARY,
            [a.clone(), b.clone()],
            open_options(),
            None,
        )
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    // Edit a cell and attach a working-copy directory (simulating structural Undo/Redo storage).
    let working_root = {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        let src = doc.source_cell(1, 1).unwrap().to_vec();
        doc.begin_cell_edit(1, 1, src).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "edited-value".into();
        doc.commit_cell_edit();
        let wc = WorkingCopyState::new_in_with_cleanup(scratch.path(), doc.working_cleanup.clone())
            .unwrap();
        let root = wc.directory.path().to_path_buf();
        doc.working_copy = Some(wc);
        root
    };
    assert!(working_root.exists());

    // 1. Simulated disk-full refuses dirty dormancy and preserves the open dirty tab intact.
    scheduler.config_mut().simulated_scratch_available_bytes = Some(1024);
    let disk_err = scheduler
        .activate_tab(WindowId::PRIMARY, ids[1])
        .unwrap_err();
    assert!(matches!(disk_err, SchedulerError::ScratchDiskFull { .. }));
    assert_eq!(scheduler.residency(ids[0]), Some(ResidencyState::Hydrated));
    assert!(working_root.exists());

    // 2. Overlay budget exceeded refuses dirty dormancy without losing edits.
    scheduler.config_mut().simulated_scratch_available_bytes = None;
    scheduler.config_mut().max_dirty_overlay_bytes = 8;
    let overlay_err = scheduler
        .activate_tab(WindowId::PRIMARY, ids[1])
        .unwrap_err();
    assert!(matches!(
        overlay_err,
        SchedulerError::OverlayBudgetExceeded { .. }
    ));
    assert_eq!(scheduler.residency(ids[0]), Some(ResidencyState::Hydrated));

    // 3. With adequate overlay budget and disk headroom, dirty tab goes dormant cleanly while
    // keeping its working directory alive and reserved against external reuse.
    scheduler.config_mut().max_dirty_overlay_bytes = 64 * 1024;
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantDirty)
    );
    assert!(
        working_root.exists(),
        "dormant dirty tab must protect working copy dir"
    );
    assert!(scheduler.path_reserved(&working_root));

    // 4. Cancelled quit retains all tabs and the dormant dirty working directory.
    scheduler.begin_quit_review();
    scheduler.approve_tab_for_quit(ids[0]).unwrap();
    scheduler.cancel_quit();
    assert_eq!(scheduler.total_tabs(), 2);
    assert!(working_root.exists());

    // 5. Waking the dirty tab restores edits, Undo history, and working copy cleanly.
    let woke = scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    assert!(matches!(
        woke,
        WakeOutcome::Ready {
            kind: WakeKind::DirtyRehydrated,
            ..
        }
    ));
    finish_active_indexing(&mut scheduler, ids[0]);
    let doc = scheduler.tab(ids[0]).unwrap().document.as_ref().unwrap();
    assert!(doc.is_dirty());
    assert!(doc.can_undo());
    assert_eq!(
        doc.cell_edits.get(&(1, 1)).map(|v| v.as_slice()),
        Some(b"edited-value".as_slice())
    );
    assert!(doc.working_copy.is_some());

    // 6. Completed quit removes Quarry's private working copy directory with zero orphans.
    let cleanup_errors = scheduler.finalize_quit();
    assert!(cleanup_errors.is_empty());
    assert!(
        !working_root.exists(),
        "completed quit must remove private working dir"
    );
}

#[test]
fn all_pinned_tabs_preserve_open_documents_and_explain_why_next_open_must_wait() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "pin-a.csv", 10);
    let b = write_csv(dir.path(), "pin-b.csv", 10);
    let c = write_csv(dir.path(), "pin-c.csv", 10);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 2,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [a, b, c], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    // Pin tab 0 with an active Join reservation and tab 1 with an open close confirmation.
    scheduler
        .tab(ids[0])
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .join_reservations
        .fetch_add(1, Ordering::AcqRel);
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    scheduler.tab_mut(ids[1]).unwrap().close_confirmation_open = true;

    assert_eq!(scheduler.pin_reason(ids[0]), Some(PinReason::JoinReserved));
    assert_eq!(scheduler.hydrated_count(), 2);

    // Attempting to activate tab 2 when all hydrated tabs are pinned must fail cleanly
    // while retaining both pinned tabs untouched.
    let err = scheduler
        .activate_tab(WindowId::PRIMARY, ids[2])
        .unwrap_err();
    assert!(matches!(
        err,
        SchedulerError::AllTabsPinned {
            hydrated: 2,
            limit: 2,
            ..
        }
    ));
    assert_eq!(scheduler.residency(ids[0]), Some(ResidencyState::Hydrated));
    assert_eq!(scheduler.residency(ids[1]), Some(ResidencyState::Hydrated));
    assert_eq!(scheduler.residency(ids[2]), Some(ResidencyState::Stub));
}

#[test]
fn active_filter_dormancy_preserves_rules_and_rebuilds_deterministically_on_wake() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "filter-a.csv", 40);
    let b = write_csv(dir.path(), "filter-b.csv", 20);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [a, b], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    let query = FilterQuery::single(1, FilterOperator::Equals, b"alpha".to_vec());
    scheduler
        .tab_mut(ids[0])
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .start_filter(query.clone())
        .unwrap();
    finish_active_filter(&mut scheduler, ids[0]);
    let expected_matches = scheduler
        .tab(ids[0])
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .available_filter_rows();
    assert_eq!(expected_matches, 10);

    // Evict filtered tab `a` by switching to `b`.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantClean)
    );

    // Wake `a`: reports `FilteredRebuilding` and deterministically rebuilds the same 10 matches.
    let outcome = scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    assert!(matches!(
        outcome,
        WakeOutcome::Ready {
            kind: WakeKind::FilteredRebuilding,
            ..
        }
    ));
    finish_active_indexing(&mut scheduler, ids[0]);
    finish_active_filter(&mut scheduler, ids[0]);
    let doc = scheduler.tab(ids[0]).unwrap().document.as_ref().unwrap();
    assert_eq!(doc.filter_query.as_ref(), Some(&query));
    assert_eq!(doc.available_filter_rows(), expected_matches);
}

#[test]
fn gzip_tabs_honor_phase_rules_and_preview_never_restarts_decompression_on_wake() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let gz_complete = write_gzip_csv(dir.path(), "complete.csv.gz", 30);
    let gz_preview = write_gzip_csv(dir.path(), "preview.csv.gz", 30);
    let plain = write_csv(dir.path(), "plain.csv", 15);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(
            WindowId::PRIMARY,
            [gz_complete.clone(), gz_preview.clone(), plain.clone()],
            open_options(),
            None,
        )
        .unwrap();

    // Wait for `gz_complete` decompression to finish (`Phase::Complete`).
    finish_active_indexing(&mut scheduler, ids[0]);
    assert_eq!(
        scheduler
            .tab(ids[0])
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .compressed
            .as_ref()
            .unwrap()
            .dormancy_policy(),
        CompressedDormancyPolicy::DormantKeepPrivateCopy
    );
    let private_copy_path = scheduler
        .tab(ids[0])
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .session
        .path()
        .to_path_buf();
    assert!(private_copy_path.exists());

    // Switch to `plain`, demoting `gz_complete` to `DormantCompressedComplete`.
    // The private decompressed copy must stay alive, reserved on disk, and charged in FdBudgetLedger.
    scheduler.activate_tab(WindowId::PRIMARY, ids[2]).unwrap();
    finish_active_indexing(&mut scheduler, ids[2]);
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantCompressedComplete)
    );
    assert!(private_copy_path.exists());
    assert!(scheduler.path_reserved(&private_copy_path));
    assert_eq!(
        scheduler
            .fd_budget()
            .snapshot()
            .by_category
            .get(&FdCategory::TempDirectory),
        Some(&1)
    );

    // Wake `gz_complete`: restores from the retained private copy without re-decompressing
    // and releases its dormant TempDirectory lease.
    let woke_complete = scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    assert_eq!(
        scheduler
            .fd_budget()
            .snapshot()
            .by_category
            .get(&FdCategory::TempDirectory),
        None
    );
    assert!(matches!(
        woke_complete,
        WakeOutcome::Ready {
            kind: WakeKind::CompressedCompleteRehydrated,
            ..
        }
    ));
    finish_active_indexing(&mut scheduler, ids[0]);
    assert_eq!(
        scheduler
            .tab(ids[0])
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .session
            .path(),
        private_copy_path
    );

    // Open `gz_preview`, cancel decompression to enter `Phase::Preview`, then cycle dormancy.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    {
        let doc = scheduler
            .tab_mut(ids[1])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.cancel_decompression();
    }
    finish_active_indexing(&mut scheduler, ids[1]);
    // Ensure we are in Preview mode (if decompression raced ahead on a tiny file, cancel before
    // wait might have completed; either way verify the exact policy contract).
    let policy = scheduler
        .tab(ids[1])
        .unwrap()
        .document
        .as_ref()
        .unwrap()
        .compressed
        .as_ref()
        .unwrap()
        .dormancy_policy();
    scheduler.activate_tab(WindowId::PRIMARY, ids[2]).unwrap();
    finish_active_indexing(&mut scheduler, ids[2]);
    if policy == CompressedDormancyPolicy::DormantPreviewOnly {
        assert_eq!(
            scheduler.residency(ids[1]),
            Some(ResidencyState::DormantCompressedPreview)
        );
        let woke_preview = scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
        assert!(matches!(
            woke_preview,
            WakeOutcome::Ready {
                kind: WakeKind::CompressedPreviewRestored,
                ..
            }
        ));
        let doc = scheduler.tab(ids[1]).unwrap().document.as_ref().unwrap();
        assert!(
            !doc.decompressing() && !doc.awaiting_decompression(),
            "Preview tab must never silently restart decompression on wake"
        );
    }
}

#[test]
fn external_source_change_while_dormant_surfaces_conflict_instead_of_silent_reload() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "mutated-a.csv", 15);
    let b = write_csv(dir.path(), "stable-b.csv", 15);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [a.clone(), b], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    // Demote `a` to dormant.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantClean)
    );

    // Mutate `a` externally on disk while dormant.
    fs::write(&a, b"id,category,amount,notes\n999,changed,1,external\n").unwrap();

    // Waking `a` must surface an explicit conflict instead of silently reloading.
    let outcome = scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    assert!(matches!(
        outcome,
        WakeOutcome::SourceChangedConflict { ref notice, .. } if notice == SOURCE_CHANGED_NOTICE
    ));
    let tab = scheduler.tab(ids[0]).unwrap();
    assert!(tab.document.as_ref().unwrap().source_changed);
    assert!(
        tab.notice
            .as_ref()
            .is_some_and(|msg| msg.text == SOURCE_CHANGED_NOTICE)
    );
}

#[test]
fn pending_inline_draft_counts_toward_overlay_budget_before_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "draft-a.csv", 10);
    let b = write_csv(dir.path(), "draft-b.csv", 10);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        max_dirty_overlay_bytes: 128,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [a, b], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    // Stage an uncommitted inline cell draft whose committed size exceeds the 128-byte cap.
    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        let src = doc.source_cell(1, 1).unwrap().to_vec();
        doc.begin_cell_edit(1, 1, src).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "x".repeat(200);
    }

    // Switching away must reject eviction BEFORE demoting the document so the user keeps access.
    let err = scheduler
        .activate_tab(WindowId::PRIMARY, ids[1])
        .unwrap_err();
    assert!(matches!(err, SchedulerError::OverlayBudgetExceeded { .. }));
    assert_eq!(scheduler.residency(ids[0]), Some(ResidencyState::Hydrated));
    assert_eq!(scheduler.active_document(WindowId::PRIMARY), Some(ids[0]));
    assert!(
        scheduler
            .tab(ids[0])
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .cell_edit
            .is_some()
    );
}

#[test]
fn failed_hydration_restores_previously_active_tab_and_removes_failed_open_stub() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "valid-a.csv", 10);
    let missing = dir.path().join("does-not-exist.csv");

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let outcome = scheduler
        .open_path(WindowId::PRIMARY, a, open_options(), None)
        .unwrap();
    let a_id = match outcome {
        WakeOutcome::Ready { document_id, .. } => document_id,
        other => panic!("expected Ready, got {other:?}"),
    };
    finish_active_indexing(&mut scheduler, a_id);

    // Opening a nonexistent file must fail without leaving the window empty or dropping `a`.
    let err = scheduler
        .open_path(WindowId::PRIMARY, missing, open_options(), None)
        .unwrap_err();
    assert!(matches!(err, SchedulerError::OpenFailed(_)));
    assert_eq!(scheduler.total_tabs(), 1);
    assert_eq!(scheduler.active_document(WindowId::PRIMARY), Some(a_id));
    assert_eq!(scheduler.residency(a_id), Some(ResidencyState::Hydrated));
}

#[test]
fn clean_document_dormancy_preserves_redo_history_across_wake() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "redo-a.csv", 10);
    let b = write_csv(dir.path(), "redo-b.csv", 10);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [a, b], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    // Edit a cell and undo it: document is clean (`!is_dirty()`), yet `can_redo()` is true.
    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        let src = doc.source_cell(1, 1).unwrap().to_vec();
        doc.begin_cell_edit(1, 1, src).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "redo-me".into();
        doc.commit_cell_edit();
        let undone = doc.edit_history.undo.pop_back().unwrap();
        doc.set_edit_value(undone.target, undone.before.clone());
        doc.edit_history.redo.push(undone);
        assert!(!doc.is_dirty());
        assert!(doc.can_redo());
    }

    // Evict clean `a` to DormantClean, then wake it and verify redo still works.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantClean)
    );

    scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);
    let doc = scheduler
        .tab_mut(ids[0])
        .unwrap()
        .document
        .as_mut()
        .unwrap();
    assert!(doc.can_redo(), "redo history must survive clean dormancy");
    let redone = doc.edit_history.redo.pop().unwrap();
    doc.set_edit_value(redone.target, redone.after.clone());
    doc.edit_history.undo.push_back(redone);
    assert_eq!(
        doc.cell_edits.get(&(1, 1)).map(|v| v.as_slice()),
        Some(b"redo-me".as_slice())
    );
}

#[test]
fn dormant_working_directories_remain_charged_against_fd_budget() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "wc-a.csv", 10);
    let b = write_csv(dir.path(), "wc-b.csv", 10);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [a, b], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        let wc = WorkingCopyState::new_in_with_cleanup(scratch.path(), doc.working_cleanup.clone())
            .unwrap();
        doc.working_copy = Some(wc);
    }

    // Demoting `a` must keep a TempDirectory FD lease reserved for its retained WorkingCopyStorage.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantDirty)
    );
    let snap = scheduler.fd_budget().snapshot();
    assert_eq!(snap.by_category.get(&FdCategory::TempDirectory), Some(&1));

    // Waking `a` releases the dormant TempDirectory lease cleanly.
    scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    let snap_after = scheduler.fd_budget().snapshot();
    assert_eq!(snap_after.by_category.get(&FdCategory::TempDirectory), None);
}

#[test]
fn third_concurrent_gzip_decompression_is_rejected_by_write_slot_cap() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let gz = write_gzip_csv(dir.path(), "third.csv.gz", 20);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 4,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let _slot1 = scheduler.decompression_slots().claim();
    let _slot2 = scheduler.decompression_slots().claim();
    assert!(!scheduler.write_slot_available());

    let err = scheduler
        .open_path(WindowId::PRIMARY, gz, open_options(), None)
        .unwrap_err();
    assert!(matches!(
        err,
        SchedulerError::OpenFailed(ref msg) if msg == compressed::WRITE_SLOTS_FULL
    ));
}

#[test]
fn moving_tab_between_windows_detaches_old_window_and_duplicate_lazy_open_keeps_owner() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "win-a.csv", 10);
    let b = write_csv(dir.path(), "win-b.csv", 10);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 4,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let win1 = WindowId::new(1);
    let win2 = WindowId::new(2);
    let ids1 = scheduler
        .open_many_lazy(win1, [a.clone()], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids1[0]);
    assert_eq!(scheduler.active_document(win1), Some(ids1[0]));

    // Calling open_many_lazy on win2 with `[a, b]` must NOT steal `a` from win1; win2 activates `b`.
    let ids2 = scheduler
        .open_many_lazy(win2, [a, b], open_options(), None)
        .unwrap();
    assert_eq!(ids2[0], ids1[0]);
    assert_eq!(scheduler.active_document(win1), Some(ids1[0]));
    assert_eq!(scheduler.active_document(win2), Some(ids2[1]));

    // Explicitly moving `ids1[0]` into win2 detaches it from win1 (no phantom shared active tab).
    scheduler.activate_tab(win2, ids1[0]).unwrap();
    assert_eq!(scheduler.active_document(win2), Some(ids1[0]));
    assert_ne!(scheduler.active_document(win1), Some(ids1[0]));
}

#[test]
fn unresolved_source_conflict_survives_repeated_dormancy_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "conflict-repeat-a.csv", 15);
    let b = write_csv(dir.path(), "conflict-repeat-b.csv", 15);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [a.clone(), b], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    // First dormancy + external mutation -> SourceChangedConflict.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    fs::write(&a, b"id,category,amount,notes\n999,changed,1,external\n").unwrap();
    let first_wake = scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    assert!(matches!(
        first_wake,
        WakeOutcome::SourceChangedConflict { .. }
    ));

    // Second dormancy cycle without explicit reload must STILL surface SourceChangedConflict!
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    let second_wake = scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    assert!(matches!(
        second_wake,
        WakeOutcome::SourceChangedConflict { .. }
    ));
    assert!(
        scheduler
            .tab(ids[0])
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .source_changed
    );
}

#[test]
fn filtered_wake_preserves_selection_and_pending_materialization_pins_tab() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "sel-filter-a.csv", 30);
    let b = write_csv(dir.path(), "sel-filter-b.csv", 15);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [a, b], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    // 1. Verify pending_materialization pins the tab so Split storage review cannot be lost.
    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.pending_materialization = Some((
            ColumnTransformation::Split {
                source_column: 0,
                separator: b"-".to_vec(),
                output_count: 2,
                output_headers: None,
            },
            std::collections::BTreeSet::from([0]),
            64,
        ));
    }
    let pin_err = scheduler
        .activate_tab(WindowId::PRIMARY, ids[1])
        .unwrap_err();
    assert!(matches!(pin_err, SchedulerError::AllTabsPinned { .. }));
    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.pending_materialization = None;
    }

    // 2. Verify active filter wake preserves selection and scroll_points after start_filter.
    let query = FilterQuery::single(1, FilterOperator::Equals, b"alpha".to_vec());
    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.start_filter(query).unwrap();
    }
    finish_active_filter(&mut scheduler, ids[0]);
    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.selection = Some(GridSelection::Cell { row: 4, column: 1 });
        doc.scroll_points = 42.5;
    }
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    let doc = scheduler.tab(ids[0]).unwrap().document.as_ref().unwrap();
    assert_eq!(
        doc.selection,
        Some(GridSelection::Cell { row: 4, column: 1 })
    );
    assert_eq!(doc.scroll_points, 42.5);
}

#[test]
fn gzip_source_replaced_before_demotion_still_surfaces_conflict_on_wake() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let gz = write_gzip_csv(dir.path(), "mutated-before-demote.csv.gz", 20);
    let plain = write_csv(dir.path(), "other.csv", 10);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [gz, plain], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    // Replace the gzip source AFTER decompression finishes, BEFORE demoting to dormant.
    write_gzip_csv(dir.path(), "mutated-before-demote.csv.gz", 35);

    // Demote, then wake: must detect the mismatch against the original compressed backing.
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    let wake = scheduler.activate_tab(WindowId::PRIMARY, ids[0]).unwrap();
    assert!(matches!(wake, WakeOutcome::SourceChangedConflict { .. }));
    assert!(
        scheduler
            .tab(ids[0])
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .source_changed
    );
}

#[test]
fn structural_undo_snapshot_overlay_bytes_count_toward_memory_cap() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let a = write_csv(dir.path(), "snap-a.csv", 10);
    let b = write_csv(dir.path(), "snap-b.csv", 10);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        max_dirty_overlay_bytes: 256,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [a.clone(), b], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        let mut wc =
            WorkingCopyState::new_in_with_cleanup(scratch.path(), doc.working_cleanup.clone())
                .unwrap();
        let mut overlay = quarry_workbench::StructuralOverlay::default();
        overlay
            .cell_edits
            .insert((1, 0), std::sync::Arc::new(vec![b'z'; 512]));
        wc.undo = Some(quarry_workbench::WorkingCopySnapshot {
            path: a,
            overlay,
            edit_history: quarry_workbench::EditHistory::default(),
        });
        doc.working_copy = Some(wc);
        assert!(quarry_workbench::document_overlay_bytes(doc) >= 512);
    }

    // Even with an empty current cell_edits map, the 512-byte structural undo snapshot
    // exceeds the 256-byte cap and must reject dormancy while preserving the hydrated tab.
    let err = scheduler
        .activate_tab(WindowId::PRIMARY, ids[1])
        .unwrap_err();
    assert!(matches!(err, SchedulerError::OverlayBudgetExceeded { .. }));
    assert_eq!(scheduler.residency(ids[0]), Some(ResidencyState::Hydrated));
}

#[test]
fn completed_duplicate_preview_pins_tab_until_confirmed_or_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let dup_path = dir.path().join("duplicates.csv");
    fs::write(
        &dup_path,
        b"id,category,amount\n1,alpha,10\n1,alpha,10\n2,beta,20\n",
    )
    .unwrap();
    let other = write_csv(dir.path(), "other-dup.csv", 10);

    let mut scheduler = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 1,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let ids = scheduler
        .open_many_lazy(WindowId::PRIMARY, [dup_path, other], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler, ids[0]);

    {
        let doc = scheduler
            .tab_mut(ids[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        doc.start_find_duplicates_target(
            DuplicateTarget::Columns(vec![0]),
            CaseSensitivity::Sensitive,
        )
        .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        scheduler.poll_all();
        let ready = {
            let doc = scheduler
                .tab_mut(ids[0])
                .unwrap()
                .document
                .as_mut()
                .unwrap();
            let _ = doc.poll_structural_edit().unwrap();
            matches!(
                doc.structural_job,
                Some(StructuralJob::DuplicatePreview { .. })
            )
        };
        if ready {
            break;
        }
        assert!(Instant::now() < deadline, "duplicate preview timed out");
        std::thread::sleep(Duration::from_millis(5));
    }

    // While the duplicate preview awaits user confirmation, switching tabs under
    // hydration pressure must refuse eviction rather than discarding the preview.
    let err = scheduler
        .activate_tab(WindowId::PRIMARY, ids[1])
        .unwrap_err();
    assert!(matches!(err, SchedulerError::AllTabsPinned { .. }));
    assert!(matches!(
        scheduler
            .tab(ids[0])
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .structural_job,
        Some(StructuralJob::DuplicatePreview { .. })
    ));

    // Also verify pin_reason directly when the tab is backgrounded under a 2-tab budget.
    let mut scheduler2 = WorkbenchScheduler::new(WorkbenchSchedulerConfig {
        max_hydrated_tabs: 2,
        working_directory: scratch.path().to_path_buf(),
        index_cache_enabled: false,
        ..Default::default()
    });
    let dup_path2 = dir.path().join("duplicates-2.csv");
    fs::write(
        &dup_path2,
        b"id,category,amount\n1,alpha,10\n1,alpha,10\n2,beta,20\n",
    )
    .unwrap();
    let other2 = write_csv(dir.path(), "other-dup-2.csv", 10);
    let ids2 = scheduler2
        .open_many_lazy(WindowId::PRIMARY, [dup_path2, other2], open_options(), None)
        .unwrap();
    finish_active_indexing(&mut scheduler2, ids2[0]);
    scheduler2
        .tab_mut(ids2[0])
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .start_find_duplicates_target(
            DuplicateTarget::Columns(vec![0]),
            CaseSensitivity::Sensitive,
        )
        .unwrap();
    let deadline2 = Instant::now() + Duration::from_secs(10);
    loop {
        let doc = scheduler2
            .tab_mut(ids2[0])
            .unwrap()
            .document
            .as_mut()
            .unwrap();
        let _ = doc.poll_structural_edit().unwrap();
        if matches!(
            doc.structural_job,
            Some(StructuralJob::DuplicatePreview { .. })
        ) {
            break;
        }
        assert!(Instant::now() < deadline2, "duplicate preview 2 timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
    scheduler2.activate_tab(WindowId::PRIMARY, ids2[1]).unwrap();
    finish_active_indexing(&mut scheduler2, ids2[1]);
    assert_eq!(
        scheduler2.pin_reason(ids2[0]),
        Some(PinReason::PendingDuplicatePreview)
    );

    // Cancelling the duplicate preview releases the pin and allows normal clean dormancy.
    scheduler
        .tab_mut(ids[0])
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .cancel_structural_edit();
    scheduler.activate_tab(WindowId::PRIMARY, ids[1]).unwrap();
    finish_active_indexing(&mut scheduler, ids[1]);
    assert_eq!(
        scheduler.residency(ids[0]),
        Some(ResidencyState::DormantClean)
    );
}
