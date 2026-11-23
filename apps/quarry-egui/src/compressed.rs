//! Read-only documents opened from gzip-compressed files.
//!
//! The first rows come from a bounded in-memory bootstrap and show at once. One
//! background pass then writes a private decompressed copy and indexes it; the
//! document adopts that session and index when the pass ends, with no second scan.
//! Cancelling, or a failure, removes the copy and leaves only the bootstrap rows as a
//! read-only preview. Every mutating command is unavailable. Save As writes an
//! uncompressed copy, which then opens as an ordinary, editable document.

use super::*;
#[cfg(test)]
pub(super) use quarry_workbench::compressed::{
    CANCELLED, DECOMPRESSING, FAILED_PREVIEW, JOIN_READ_ONLY, PREVIEW_ONLY, READ_ONLY,
    UNREADABLE_PREVIEW,
};
pub(super) use quarry_workbench::compressed::{
    COMPRESSED_DESTINATION, DecompressionSlot, DecompressionSlots, WRITE_SLOTS_FULL,
    compressed_source, decompression_status, gzip_destination_warning, is_gzip, uncompressed_name,
};

/// Footer progress for decompression, which runs before any other operation.
pub(super) fn decompression_display(document: &Document) -> Option<ActiveJobDisplay> {
    let status = decompression_status(document)?;
    Some(ActiveJobDisplay {
        label: status.label,
        fraction: status.fraction,
        animate: false,
        cancel_action: Action::Cancel,
        cancel_label: "Cancel",
        cancel_enabled: status.cancel_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    // Worker checkpoints exist only in debug builds of quarry-core.
    #[cfg(debug_assertions)]
    use quarry_core::compressed_test_pause::{Checkpoint, Pause};
    use quarry_core::{CaseSensitivity, FilterOperator, FilterQuery};
    use std::fs;
    use std::io::Write;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    /// `name,value,double` with `rows` numbered data rows.
    fn csv(rows: usize) -> Vec<u8> {
        let mut bytes = b"name,value,double\n".to_vec();
        for row in 1..=rows {
            bytes.extend_from_slice(format!("row{row},{row},{}\n", row * 2).as_bytes());
        }
        bytes
    }

    /// One small member repeated: tiny on disk, `members` MiB once expanded.
    fn large_gzip(path: &Path, members: usize, trailing: &[u8]) {
        let mut block = b"id,name,amount\n".to_vec();
        while block.len() < (1 << 20) - 64 {
            block.extend_from_slice(b"1,customer,7.00\n");
        }
        let member = gzip(&block);
        let mut file = fs::File::create(path).unwrap();
        for _ in 0..members {
            file.write_all(&member).unwrap();
        }
        file.write_all(trailing).unwrap();
    }

    /// Names of private decompressed copies left in the working folder.
    fn leftovers(directory: &Path) -> Vec<String> {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".quarry-compressed-"))
            .collect()
    }

    fn open_document(path: &Path, scratch: &Path) -> Document {
        let mut document =
            Document::prepare_with_encoding(path, OpenOptions::default(), None).unwrap();
        document.working_directory = scratch.to_path_buf();
        document.start_indexing().unwrap();
        document
    }

    fn finish(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while document.decompressing() || document.job.is_some() {
            assert!(Instant::now() < deadline, "decompression timed out");
            document.poll().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn app_with(path: &Path, scratch: &Path) -> (QuarryApp, egui::Context) {
        let mut app = QuarryApp::new(None, Instant::now());
        app.working_directory = scratch.to_path_buf();
        app.open_path(path.to_path_buf()).unwrap();
        (app, egui::Context::default())
    }

    fn settle(app: &mut QuarryApp, ctx: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.document_tabs().any(DocumentTab::busy) {
            assert!(Instant::now() < deadline, "tab jobs timed out");
            app.poll_document_tabs(ctx);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn active(app: &mut QuarryApp) -> &mut Document {
        app.active.document.as_mut().unwrap()
    }

    /// Holds a worker once it has published about 4,000 rows of `large_gzip` data.
    #[cfg(debug_assertions)]
    const HELD: Checkpoint = Checkpoint::Published {
        min_bytes: 64 * 1024,
    };

    /// `count` four-member `large_gzip` sources, each about 4 MiB once expanded.
    #[cfg(debug_assertions)]
    fn large_sources(directory: &Path, count: usize) -> Vec<PathBuf> {
        (0..count)
            .map(|index| {
                let path = directory.join(format!("large-{index}.csv.gz"));
                large_gzip(&path, 4, &[]);
                path
            })
            .collect()
    }

    #[cfg(debug_assertions)]
    fn close_all(app: &mut QuarryApp, ctx: &egui::Context) {
        while app.active.document.is_some() {
            app.request_tab_close(ctx);
            crate::tests::finish_document_cleanup(app, ctx);
        }
    }

    #[test]
    fn suggested_names_drop_the_compressed_extension() {
        for (source, save_as, export) in [
            ("/data/sales.csv.gz", "sales.csv", "sales-filtered.csv"),
            ("/data/sales.tsv.gz", "sales.tsv", "sales-filtered.tsv"),
            ("/data/SALES.CSV.GZ", "SALES.CSV", "SALES-filtered.CSV"),
            ("/data/data.gz", "data.csv", "data-filtered.csv"),
            ("/data/sales.csv", "sales-edited.csv", "sales-filtered.csv"),
        ] {
            assert_eq!(save_as_file_name(Path::new(source)), save_as, "{source}");
            assert_eq!(
                filtered_export_file_name(Path::new(source)),
                export,
                "{source}"
            );
        }
        for name in ["/out/copy.csv.GZ", "/out/copy.tgz", "/out/COPY.TGZ"] {
            assert!(
                gzip_destination_warning(Path::new(name)).is_some(),
                "{name}"
            );
        }
        assert!(gzip_destination_warning(Path::new("/out/copy.csv")).is_none());
    }

    #[test]
    fn single_and_multi_member_files_show_rows_first_then_the_full_index() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let data = csv(500);
        let single = directory.path().join("single.csv.gz");
        fs::write(&single, gzip(&data)).unwrap();
        let multi = directory.path().join("multi.csv.gz");
        let (first, second) = data.split_at(data.len() / 3);
        fs::write(&multi, [gzip(first), gzip(second)].concat()).unwrap();

        for path in [&single, &multi] {
            let mut document =
                Document::prepare_with_encoding(path, OpenOptions::default(), None).unwrap();
            // The bootstrap rows show before any temporary file exists.
            assert!(document.is_compressed() && document.decompressing());
            assert!(document.session.path().as_os_str().is_empty());
            assert_eq!(document.visible_row(0).unwrap().0, 1);
            assert_eq!(document.visible_row(0).unwrap().1[0], b"row1");
            assert_eq!(document.headers, ["name", "value", "double"]);
            assert_eq!(
                document.compressed_unavailable_reason(),
                Some(DECOMPRESSING)
            );
            assert_eq!(document.index_status(), "Decompressing");
            assert!(leftovers(scratch.path()).is_empty());

            document.working_directory = scratch.path().to_path_buf();
            document.start_indexing().unwrap();
            assert!(decompression_display(&document).is_some());
            finish(&mut document);
            assert!(!document.decompressing());
            assert_eq!(document.index.as_ref().unwrap().indexed_rows(), 501);
            assert_eq!(document.available_data_rows(), 500);
            assert!(document.width_confirmed());
            assert_eq!(document.known_width(), 3);
            assert_eq!(
                document.session.compressed_source_path(),
                Some(path.as_path())
            );
            assert_eq!(document.logical_path, *path);
            assert_eq!(document.index_status(), "Index complete");
            let label = document.compressed_label().unwrap();
            assert!(label.starts_with("Gzip (read-only) · "), "{label}");
            assert!(!label.contains("preview") && !label.contains("decompressing"));
            document.navigate(400).unwrap();
            let (row, fields) = document.visible_row(0).unwrap();
            assert_eq!(row, 400);
            assert_eq!(
                fields,
                [b"row400".to_vec(), b"400".to_vec(), b"800".to_vec()]
            );
            assert_eq!(leftovers(scratch.path()).len(), 1);
            drop(document);
            assert!(leftovers(scratch.path()).is_empty());
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    fn cancelling_keeps_preview_rows_and_removes_the_private_copy() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let source = directory.path().join("large.csv.gz");
        large_gzip(&source, 8, &[]);
        // The worker is held after publishing about 65,000 rows, so the live read and
        // the cancel below always happen while decompression is still running.
        let pause = Pause::new(&source, Checkpoint::Published { min_bytes: 1 << 20 });
        let (mut app, ctx) = app_with(&source, scratch.path());
        let first_rows = active(&mut app).session.first_rows.len() as u64;
        assert_eq!(leftovers(scratch.path()).len(), 1);
        assert!(pause.wait_for(1), "no rows were decompressed");

        // Rows beyond the bootstrap come from the copy while it is being written.
        let deadline = Instant::now() + Duration::from_secs(20);
        while active(&mut app).progress.rows_scanned < 20_000 {
            assert!(Instant::now() < deadline, "no rows were decompressed");
            active(&mut app).poll().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        active(&mut app).navigate(10_000).unwrap();
        let (row, fields) = active(&mut app).visible_row(0).unwrap();
        assert_eq!(row, 10_000);
        assert_eq!(
            fields,
            [b"1".to_vec(), b"customer".to_vec(), b"7.00".to_vec()]
        );
        assert!(!active(&mut app).is_save_as_ready());
        let label = active_job_display(active(&mut app)).unwrap();
        assert!(label.label.starts_with("Decompressing"), "{}", label.label);
        assert_eq!(label.cancel_action, Action::Cancel);

        app.apply(&ctx, Action::Cancel);
        assert_eq!(app.active.notice.as_ref().unwrap().text, CANCELLED);
        assert!(leftovers(scratch.path()).is_empty());
        let document = active(&mut app);
        assert!(!document.decompressing());
        assert!(document.index.is_none());
        assert_eq!(document.compressed_unavailable_reason(), Some(PREVIEW_ONLY));
        assert_eq!(document.available_rows(), first_rows);
        let (row, _) = document.visible_row(0).unwrap();
        assert!(row < first_rows, "the preview shows only bootstrap rows");
        assert_eq!(document.index_status(), "Preview only");
        assert!(
            document
                .compressed_label()
                .unwrap()
                .ends_with("preview only")
        );
        assert!(!document.is_save_as_ready() && !document.is_save_ready());
        assert_eq!(
            document.start_find_next_with_case(b"customer", CaseSensitivity::Insensitive),
            Err(PREVIEW_ONLY.into())
        );
        assert_eq!(
            document.start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"1".to_vec()
            )),
            Err(PREVIEW_ONLY.into())
        );
        app.start_current_inspection();
        assert_eq!(app.active.notice.as_ref().unwrap().text, PREVIEW_ONLY);
        assert!(!app.active.inspection.open);

        // Reload reads the compressed source again; the new pass is held as well.
        app.reload_document();
        assert!(active(&mut app).decompressing());
        assert!(pause.wait_for(2), "the reloaded pass did not start");
        assert_eq!(leftovers(scratch.path()).len(), 1);
        app.request_tab_close(&ctx);
        crate::tests::finish_document_cleanup(&mut app, &ctx);
        assert!(app.active.document.is_none());
        assert!(leftovers(scratch.path()).is_empty());
    }

    #[test]
    fn damaged_files_fail_with_an_explicit_error_and_leave_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let valid = gzip(&csv(20));
        let mut bad_crc = valid.clone();
        let at = valid.len() - 8;
        bad_crc[at] ^= 0xff;
        let truncated = valid[..valid.len() - 4].to_vec();
        for (name, bytes) in [("crc.csv.gz", bad_crc), ("short.csv.gz", truncated)] {
            let path = directory.path().join(name);
            fs::write(&path, bytes).unwrap();
            let mut app = QuarryApp::new(None, Instant::now());
            app.working_directory = scratch.path().to_path_buf();
            let error = app
                .open_document_tab(path.clone(), OpenOptions::default())
                .unwrap_err();
            assert_eq!(error.severity, MessageSeverity::Error);
            assert!(
                error.inspection_target.is_none(),
                "{name}: raw inspection offered"
            );
            assert!(app.active.document.is_none());
            assert!(!app.active.inspection.open);
            assert!(leftovers(scratch.path()).is_empty());
            // A dropped or Finder-opened file names its path once, not twice.
            app.handle_dropped_paths(vec![Some(path.clone())]);
            let notice = app.active.notice.take().expect("a failure notice");
            let shown = path.display().to_string();
            assert!(notice.text.starts_with(&shown), "{name}: {}", notice.text);
            assert_eq!(
                notice.text.matches(&shown).count(),
                1,
                "{name}: {}",
                notice.text
            );
        }

        // Damage after the bootstrap fails the background pass.
        let late = directory.path().join("late.csv.gz");
        large_gzip(&late, 4, &gzip(b"1,customer,7.00\n")[..10]);
        let mut document = open_document(&late, scratch.path());
        let deadline = Instant::now() + Duration::from_secs(20);
        let error = loop {
            assert!(Instant::now() < deadline, "damage was not detected");
            if let Err(error) = document.poll() {
                break error;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(error.contains("truncated"), "{error}");
        assert!(error.ends_with(FAILED_PREVIEW), "{error}");
        assert!(!document.decompressing() && document.index.is_none());
        assert_eq!(document.compressed_unavailable_reason(), Some(PREVIEW_ONLY));
        assert!(leftovers(scratch.path()).is_empty());
    }

    #[test]
    fn compressed_documents_are_read_only_but_save_as_needs_no_edits() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let source = directory.path().join("sales.csv.gz");
        fs::write(&source, gzip(&csv(30))).unwrap();
        let mut document = open_document(&source, scratch.path());
        assert!(!document.is_save_as_ready());
        assert_eq!(
            document
                .start_save_as(directory.path().join("early.csv"))
                .unwrap_err()
                .text,
            DECOMPRESSING
        );
        assert_eq!(
            document.start_find_next_with_case(b"row", CaseSensitivity::Insensitive),
            Err(DECOMPRESSING.into())
        );
        finish(&mut document);

        assert!(!document.is_dirty() && !document.has_structural_edits());
        assert!(!document.is_save_ready());
        assert!(document.is_save_as_ready());
        let commands = FileCommands::new(Some(&document), true);
        assert!(!commands.save && commands.save_as && !commands.discard && commands.reload);
        assert_eq!(document.start_save().unwrap_err().text, READ_ONLY);
        assert_eq!(
            document.cell_edit_disabled_reason(Some(b"row1")),
            Some(READ_ONLY)
        );
        assert_eq!(
            document.begin_cell_edit(1, 0, b"row1".to_vec()),
            Err(READ_ONLY.into())
        );
        assert!(!document.header_is_editable(0));
        assert_eq!(
            document.rename_header(0, "renamed".into()),
            Err(READ_ONLY.into())
        );
        assert_eq!(document.structural_edit_disabled_reason(), Some(READ_ONLY));
        assert_eq!(document.insertion_disabled_reason(true), Some(READ_ONLY));
        assert_eq!(document.insertion_disabled_reason(false), Some(READ_ONLY));
        assert_eq!(
            document
                .start_delete_rows(std::iter::once(1..=1).collect())
                .unwrap_err()
                .text,
            READ_ONLY
        );
        assert_eq!(
            document.start_delete_columns(vec![0]).unwrap_err().text,
            READ_ONLY
        );
        assert_eq!(
            document.start_split(0, b"o".to_vec()).unwrap_err().text,
            READ_ONLY
        );
        assert_eq!(
            document.start_move_columns(vec![0], 2).unwrap_err().text,
            READ_ONLY
        );
        assert!(document.start_replace_all(b"row", b"line").is_err());
        assert!(!document.can_replace_current(b"row"));
        assert!(!document.can_undo() && !document.can_redo());
        assert_eq!(document.compressed_join_reason(), Some(JOIN_READ_ONLY));
        // Viewing changes stay available.
        document.move_column(2, 0).unwrap();
        document.set_column_shown(1, false).unwrap();
        assert!(!document.is_dirty());
        assert!(document.is_save_as_ready());
        assert!(leftovers(scratch.path()).len() == 1);
    }

    #[test]
    fn find_filter_export_inspect_and_save_as_work_after_decompression() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let data = csv(300);
        let source = directory.path().join("sales.csv.gz");
        fs::write(&source, gzip(&data)).unwrap();
        let (mut app, ctx) = app_with(&source, scratch.path());
        app.start_current_inspection();
        assert_eq!(app.active.notice.take().unwrap().text, DECOMPRESSING);
        settle(&mut app, &ctx);

        let document = active(&mut app);
        document
            .start_find_next_with_case(b"row250", CaseSensitivity::Insensitive)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while document.search_job.is_some() {
            assert!(Instant::now() < deadline, "search timed out");
            document.poll_search().unwrap();
            std::thread::yield_now();
        }
        assert_eq!(document.last_match.unwrap().row, 250);

        document
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Contains,
                b"row25".to_vec(),
            ))
            .unwrap();
        while document.filter_job.is_some() || document.filter_rows_loading() {
            assert!(Instant::now() < deadline, "filter timed out");
            document.poll_filter().unwrap();
            std::thread::yield_now();
        }
        assert_eq!(document.available_filter_rows(), 11);
        let exported = directory.path().join("filtered.csv");
        app.export_picker_result(Some(directory.path().join("filtered.csv.gz")));
        assert!(
            app.active
                .notice
                .take()
                .unwrap()
                .text
                .contains("without .gz")
        );
        assert!(!directory.path().join("filtered.csv.gz").exists());
        app.export_picker_result(Some(exported.clone()));
        settle(&mut app, &ctx);
        let mut expected = b"name,value,double\nrow25,25,50\n".to_vec();
        for row in 250..=259 {
            expected.extend_from_slice(format!("row{row},{row},{}\n", row * 2).as_bytes());
        }
        assert_eq!(fs::read(&exported).unwrap(), expected);
        active(&mut app).clear_filter().unwrap();

        app.start_current_inspection();
        assert!(app.active.notice.is_none());
        assert!(app.active.inspection.open);
        assert_eq!(app.active.inspection.display_path(), Some(source.as_path()));
        app.active.inspection = inspection_ui::InspectionUi::default();

        let gz_copy = directory.path().join("copy.csv.gz");
        assert!(!app.save_as_picker_result(Some(gz_copy.clone())));
        assert!(
            app.active
                .notice
                .take()
                .unwrap()
                .text
                .contains("without .gz")
        );
        assert!(!gz_copy.exists());
        let copy = directory.path().join("sales.csv");
        assert!(app.save_as_picker_result(Some(copy.clone())));
        settle(&mut app, &ctx);
        assert_eq!(fs::read(&copy).unwrap(), data);
        // The uncompressed copy opens in the tab as an ordinary, editable document.
        let document = active(&mut app);
        assert_eq!(document.logical_path, copy);
        assert!(!document.is_compressed());
        assert!(document.header_is_editable(0));
        assert!(leftovers(scratch.path()).is_empty());

        // Save As UTF-8 converts a legacy encoding while writing the uncompressed copy.
        let legacy = directory.path().join("legacy.csv.gz");
        fs::write(&legacy, gzip(b"name,city\nAna,M\xe1laga\n")).unwrap();
        app.open_document_tab_with_encoding(
            legacy.clone(),
            OpenOptions::default(),
            Some(Encoding::Windows1252),
        )
        .unwrap();
        settle(&mut app, &ctx);
        let utf8 = directory.path().join("legacy.csv");
        assert!(app.save_as_picker_result_format(Some(utf8.clone()), true));
        settle(&mut app, &ctx);
        assert_eq!(
            fs::read(&utf8).unwrap(),
            "name,city\nAna,Málaga\n".as_bytes()
        );
        assert_eq!(fs::read(&source).unwrap(), gzip(&data));
    }

    #[test]
    fn format_changes_reuse_the_private_copy_until_the_source_changes() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let source = directory.path().join("sales.csv.gz");
        fs::write(&source, gzip(&csv(40))).unwrap();
        let (mut app, ctx) = app_with(&source, scratch.path());
        settle(&mut app, &ctx);
        let copy = active(&mut app).session.path().to_path_buf();

        app.reopen_document_with_encoding(DelimiterMode::Auto, HeaderMode::NoHeader, None);
        assert!(app.active.notice.is_none());
        let reopened = active(&mut app);
        assert!(reopened.is_compressed() && !reopened.decompressing());
        assert_eq!(reopened.session.path(), copy);
        assert!(!reopened.session.dialect.has_header);
        settle(&mut app, &ctx);
        assert_eq!(active(&mut app).available_data_rows(), 41);
        assert!(active(&mut app).is_save_as_ready());
        assert_eq!(leftovers(scratch.path()).len(), 1);

        // A changed source is decompressed again rather than trusting the old copy.
        fs::write(&source, gzip(&csv(50))).unwrap();
        app.reopen_document_with_encoding(DelimiterMode::Comma, HeaderMode::FirstRow, None);
        assert!(active(&mut app).decompressing());
        settle(&mut app, &ctx);
        assert_ne!(active(&mut app).session.path(), copy);
        assert_eq!(active(&mut app).available_data_rows(), 50);
        assert_eq!(leftovers(scratch.path()).len(), 1);

        app.reload_document();
        settle(&mut app, &ctx);
        assert_eq!(active(&mut app).available_data_rows(), 50);
        assert_eq!(leftovers(scratch.path()).len(), 1);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn decompression_shares_the_job_limit_and_closing_removes_its_copy() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let paths = large_sources(directory.path(), 3);
        let pauses: Vec<_> = paths[..2]
            .iter()
            .map(|path| Pause::new(path, HELD))
            .collect();
        let (mut app, ctx) = app_with(&paths[0], scratch.path());
        app.open_path(paths[1].clone()).unwrap();
        assert!(pauses.iter().all(|pause| pause.wait_for(1)));
        assert!(app.document_tabs().all(DocumentTab::busy));
        assert!(!app.write_slot_available());
        assert_eq!(
            app.open_path(paths[2].clone()).unwrap_err(),
            WRITE_SLOTS_FULL
        );
        assert_eq!(leftovers(scratch.path()).len(), 2);

        // Closing a decompressing tab cancels it without waiting for the pass to end.
        app.request_tab_close(&ctx);
        crate::tests::finish_document_cleanup(&mut app, &ctx);
        assert_eq!(leftovers(scratch.path()).len(), 1);
        assert!(app.write_slot_available());
        app.request_tab_close(&ctx);
        crate::tests::finish_document_cleanup(&mut app, &ctx);
        assert!(app.active.document.is_none());
        assert!(leftovers(scratch.path()).is_empty());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn reloading_a_decompressing_tab_never_runs_a_third_decompression() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let paths = large_sources(directory.path(), 2);
        let pauses: Vec<_> = paths.iter().map(|path| Pause::new(path, HELD)).collect();
        let (mut app, ctx) = app_with(&paths[0], scratch.path());
        app.open_path(paths[1].clone()).unwrap();
        assert!(pauses.iter().all(|pause| pause.wait_for(1)));
        assert_eq!(app.decompression_slots.active(), 2);
        assert!(!app.write_slot_available());

        // The active tab stops its own pass before the reloaded pass starts.
        app.reload_document();
        assert!(
            app.active.notice.is_none(),
            "{:?}",
            app.active.notice.as_ref().map(|notice| notice.text.clone())
        );
        assert!(active(&mut app).decompressing());
        assert!(pauses[1].wait_for(2), "the reloaded pass did not start");
        assert_eq!(app.decompression_slots.active(), 2);
        assert_eq!(app.decompression_slots.peak(), 2);
        assert_eq!(leftovers(scratch.path()).len(), 2);

        // A plain file needs no slot and still opens while both are in use.
        let plain = directory.path().join("plain.csv");
        fs::write(&plain, csv(5)).unwrap();
        app.open_path(plain).unwrap();
        assert!(!active(&mut app).is_compressed());

        close_all(&mut app, &ctx);
        assert_eq!(app.decompression_slots.active(), 0);
        assert_eq!(app.decompression_slots.peak(), 2);
        assert!(leftovers(scratch.path()).is_empty());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn a_closed_tab_keeps_its_decompression_slot_until_cleanup_stops_it() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let paths = large_sources(directory.path(), 3);
        let mut app = QuarryApp::new(None, Instant::now());
        app.working_directory = scratch.path().to_path_buf();
        let ctx = egui::Context::default();
        let running = Pause::new(&paths[0], HELD);
        // The closing tab's worker stays held even once cancelled, as a slow stop
        // would, until the pause is released.
        let closing = Pause::holding_cancelled(&paths[1], HELD);
        app.open_path(paths[0].clone()).unwrap();
        app.open_path(paths[1].clone()).unwrap();
        assert!(running.wait_for(1) && closing.wait_for(1));

        // Closing returns at once; the worker is still stopping in the cleanup thread.
        app.request_tab_close(&ctx);
        assert!(app.document_cleanup.is_some());
        assert_eq!(
            app.document_tabs()
                .filter(|tab| tab.document.is_some())
                .count(),
            1
        );
        assert_eq!(app.decompression_slots.active(), 2);
        assert!(!app.write_slot_available());
        assert_eq!(
            app.open_path(paths[2].clone()).unwrap_err(),
            WRITE_SLOTS_FULL
        );

        // Once the retiring worker exits, its slot is free again.
        closing.release();
        crate::tests::finish_document_cleanup(&mut app, &ctx);
        assert_eq!(app.decompression_slots.active(), 1);
        assert!(app.write_slot_available());
        app.open_path(paths[2].clone()).unwrap();
        assert!(active(&mut app).decompressing());
        assert_eq!(app.decompression_slots.peak(), 2);

        close_all(&mut app, &ctx);
        assert_eq!(app.decompression_slots.active(), 0);
        assert!(leftovers(scratch.path()).is_empty());
    }
}
