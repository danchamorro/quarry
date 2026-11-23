//! Headless integration tests proving `quarry-workbench` command enablement,
//! window/document targeting, and the dirty-tab close + Keep Editing vertical slice
//! operate with zero GUI framework dependencies.

use std::fs;
use std::time::{Duration, Instant};

use quarry_core::{Encoding, FilterOperator, FilterQuery, HeaderMode, OpenOptions};
use quarry_workbench::{
    CloseCommandOutcome, CommandContext, CommandId, CommandTarget, Document, DocumentId,
    FileCommands, WindowId, WorkbenchTab, evaluate_command,
};

fn open_indexed_document(contents: &[u8]) -> (tempfile::TempDir, Document) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slice.csv");
    fs::write(&path, contents).unwrap();
    let mut doc = Document::open(
        &path,
        OpenOptions {
            header_mode: HeaderMode::FirstRow,
            ..Default::default()
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.job.is_some() {
        assert!(Instant::now() < deadline, "index timed out");
        doc.poll().unwrap();
        std::thread::yield_now();
    }
    (dir, doc)
}

#[test]
fn dirty_tab_close_and_keep_editing_vertical_slice_runs_headlessly() {
    let (_dir, doc) = open_indexed_document(b"name,score\nAda,10\nGrace,20\n");
    let window = WindowId::new(2);
    let mut tab = WorkbenchTab::new(42);
    assert_eq!(tab.document_id(), DocumentId::new(42));
    assert_eq!(window.next(), WindowId::new(3));
    assert_eq!(tab.document_id().next(), DocumentId::new(43));

    // Before opening a file, CloseTab has no document target and is disabled.
    let empty_ctx = CommandContext {
        window,
        active_tab: &tab,
        open_document_count: 0,
        write_slot_available: true,
        external_modal_blocked: false,
        text_field_focused: false,
        show_all_whitespace: false,
    };
    let empty_close = evaluate_command(CommandId::CloseTab, &empty_ctx);
    assert!(!empty_close.enabled);
    assert_eq!(empty_close.target, None);
    assert!(evaluate_command(CommandId::Open, &empty_ctx).enabled);
    assert_eq!(
        evaluate_command(CommandId::Open, &empty_ctx).target,
        Some(CommandTarget::Application)
    );

    // Open document and stage an unsaved inline cell edit (committed by CloseTab).
    tab.document = Some(doc);
    {
        let doc = tab.document.as_mut().unwrap();
        let source = doc.source_cell(1, 1).unwrap().to_vec();
        doc.begin_cell_edit(1, 1, source).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "999".into();
    }

    let dirty_ctx = CommandContext {
        window,
        active_tab: &tab,
        open_document_count: 1,
        write_slot_available: true,
        external_modal_blocked: false,
        text_field_focused: false,
        show_all_whitespace: false,
    };
    let close_res = evaluate_command(CommandId::CloseTab, &dirty_ctx);
    assert!(close_res.enabled);
    assert_eq!(
        close_res.target,
        Some(CommandTarget::Document {
            window,
            document: DocumentId::new(42),
        })
    );
    assert!(!evaluate_command(CommandId::KeepEditing, &dirty_ctx).enabled);
    assert!(!evaluate_command(CommandId::DiscardAndClose, &dirty_ctx).enabled);

    // 1. Dispatch CloseTab: commits the in-progress draft and requests unsaved confirmation.
    let outcome = tab
        .dispatch_close_command(CommandId::CloseTab, false, false, false)
        .unwrap();
    assert_eq!(outcome, CloseCommandOutcome::ConfirmUnsaved);
    assert!(tab.close_confirmation_open);
    assert!(tab.blocks_commands());
    assert!(tab.document.as_ref().unwrap().cell_edit.is_none());
    assert_eq!(
        tab.document
            .as_ref()
            .unwrap()
            .cell_edits
            .get(&(1, 1))
            .map(|value| value.as_slice()),
        Some(b"999".as_slice())
    );

    // While confirmation is open, regular document commands are blocked, while KeepEditing and
    // DiscardAndClose are enabled against the targeted document.
    let modal_ctx = CommandContext {
        window,
        active_tab: &tab,
        open_document_count: 1,
        write_slot_available: true,
        external_modal_blocked: false,
        text_field_focused: false,
        show_all_whitespace: false,
    };
    assert!(!evaluate_command(CommandId::Save, &modal_ctx).enabled);
    assert!(!evaluate_command(CommandId::CloseTab, &modal_ctx).enabled);
    assert!(!evaluate_command(CommandId::Undo, &modal_ctx).enabled);
    let keep = evaluate_command(CommandId::KeepEditing, &modal_ctx);
    assert!(keep.enabled);
    assert_eq!(
        keep.target,
        Some(CommandTarget::Document {
            window,
            document: DocumentId::new(42),
        })
    );
    assert!(evaluate_command(CommandId::DiscardAndClose, &modal_ctx).enabled);

    // 2. Dispatch KeepEditing: cancels confirmation, unblocks commands, and retains the edit.
    let kept = tab
        .dispatch_close_command(CommandId::KeepEditing, false, false, false)
        .unwrap();
    assert_eq!(kept, CloseCommandOutcome::KeptEditing);
    assert!(!tab.close_confirmation_open);
    assert!(!tab.close_ready);
    assert!(!tab.blocks_commands());
    assert!(tab.is_dirty());
    assert_eq!(
        tab.document
            .as_ref()
            .unwrap()
            .cell_edits
            .get(&(1, 1))
            .map(|value| value.as_slice()),
        Some(b"999".as_slice())
    );

    let resumed_ctx = CommandContext {
        window,
        active_tab: &tab,
        open_document_count: 1,
        write_slot_available: true,
        external_modal_blocked: false,
        text_field_focused: false,
        show_all_whitespace: false,
    };
    assert!(evaluate_command(CommandId::Save, &resumed_ctx).enabled);
    assert!(evaluate_command(CommandId::DiscardChanges, &resumed_ctx).enabled);
    assert!(evaluate_command(CommandId::Undo, &resumed_ctx).enabled);
    assert!(!evaluate_command(CommandId::ReloadFromDisk, &resumed_ctx).enabled);

    // 3. Re-close and confirm DiscardAndClose transitions cleanly to ReadyToClose.
    assert_eq!(
        tab.dispatch_close_command(CommandId::CloseTab, false, false, false)
            .unwrap(),
        CloseCommandOutcome::ConfirmUnsaved
    );
    assert_eq!(
        tab.dispatch_close_command(CommandId::DiscardAndClose, false, false, false)
            .unwrap(),
        CloseCommandOutcome::ReadyToClose
    );
    assert!(tab.close_ready);
    assert!(!tab.close_confirmation_open);
    drop(tab);
}

#[test]
fn command_enablement_respects_targets_filters_text_focus_and_write_slots() {
    let (_dir, mut doc) = open_indexed_document(b"city,count\nBoston,1\nAustin,2\n");
    // Apply an active filter to verify Find is disabled while filter is active.
    doc.start_filter(FilterQuery::single(
        0,
        FilterOperator::Contains,
        b"Boston".to_vec(),
    ))
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while doc.filter_job.is_some() {
        assert!(Instant::now() < deadline, "filter timed out");
        doc.poll_filter().unwrap();
        std::thread::yield_now();
    }

    let mut tab = WorkbenchTab::new(9);
    tab.document = Some(doc);

    let ctx = CommandContext {
        window: WindowId::PRIMARY,
        active_tab: &tab,
        open_document_count: 2,
        write_slot_available: false,
        external_modal_blocked: false,
        text_field_focused: false,
        show_all_whitespace: true,
    };

    assert!(!FileCommands::new(tab.document.as_ref(), false).combine);
    assert!(!evaluate_command(CommandId::CombineFiles, &ctx).enabled);
    assert!(!evaluate_command(CommandId::Find, &ctx).enabled);
    assert!(evaluate_command(CommandId::Filters, &ctx).enabled);
    assert!(evaluate_command(CommandId::NextTab, &ctx).enabled);
    assert_eq!(
        evaluate_command(CommandId::NextTab, &ctx).target,
        Some(CommandTarget::Window(WindowId::PRIMARY))
    );
    assert!(evaluate_command(CommandId::OpenWithEncoding(Encoding::Utf8), &ctx).enabled);

    // Focused text fields keep Undo/Redo enabled even while a modal blocks document commands.
    let blocked_text_ctx = CommandContext {
        window: WindowId::PRIMARY,
        active_tab: &tab,
        open_document_count: 2,
        write_slot_available: true,
        external_modal_blocked: true,
        text_field_focused: true,
        show_all_whitespace: false,
    };
    assert!(evaluate_command(CommandId::Undo, &blocked_text_ctx).enabled);
    assert!(evaluate_command(CommandId::Redo, &blocked_text_ctx).enabled);
    assert!(!evaluate_command(CommandId::Open, &blocked_text_ctx).enabled);
    assert!(
        !evaluate_command(
            CommandId::OpenWithEncoding(Encoding::Latin1),
            &blocked_text_ctx
        )
        .enabled
    );
    assert!(!evaluate_command(CommandId::NextTab, &blocked_text_ctx).enabled);
    drop(tab);
}
