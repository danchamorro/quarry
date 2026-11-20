//! Read-only documents opened from gzip-compressed files.
//!
//! The first rows come from a bounded in-memory bootstrap and show at once. One
//! background pass then writes a private decompressed copy and indexes it; the
//! document adopts that session and index when the pass ends, with no second scan.
//! Cancelling, or a failure, removes the copy and leaves only the bootstrap rows as a
//! read-only preview. Every mutating command is unavailable. Save As writes an
//! uncompressed copy, which then opens as an ordinary, editable document.

use super::*;
use quarry_core::{
    CompressedBootstrap, CompressedDecompressJob, CompressedOpenConfig, CompressedOpenOutcome,
    CompressedProgress, CompressionFormat,
};

pub(super) const READ_ONLY: &str =
    "Compressed files open read-only. Use Save As… to create an editable uncompressed copy.";
pub(super) const DECOMPRESSING: &str =
    "Wait for decompression to finish before using this command.";
pub(super) const PREVIEW_ONLY: &str = "Only the first rows of this compressed file are loaded. Use File > Reload from Disk to load the full file.";
pub(super) const WRITE_SLOTS_FULL: &str =
    "Wait for an active file operation to finish before opening another compressed file.";
pub(super) const CANCELLED: &str = "Decompression cancelled; temporary files were removed. Showing initial preview rows. Use File > Reload from Disk to load the full file.";
const JOIN_READ_ONLY: &str = "Compressed files cannot be joined directly. Use Save As… to create an uncompressed copy, then join that copy.";
const FAILED_PREVIEW: &str = "Showing the initial preview rows only; no temporary files were kept.";

/// The compressed origin of a read-only document.
pub(super) struct CompressedDocument {
    format: CompressionFormat,
    compressed_size: u64,
    phase: Phase,
}

enum Phase {
    /// Bootstrap rows are visible; decompression starts once the tab has its storage settings.
    Pending(Box<CompressedBootstrap>),
    Decompressing {
        job: CompressedDecompressJob,
        progress: CompressedProgress,
    },
    /// The session reads the verified private copy.
    Complete,
    /// Cancelled or failed: only the bootstrap rows remain, and no private copy is kept.
    Preview,
}

/// True for gzip data. Unsupported or damaged compressed files take the ordinary open
/// path, whose error names the problem.
pub(super) fn is_gzip(path: &Path) -> bool {
    matches!(
        quarry_core::detect_compression(path),
        Ok(Some(CompressionFormat::Gzip))
    )
}

/// True when a failed open concerns compressed data, which raw inspection cannot read.
pub(super) fn compressed_source(path: &Path) -> bool {
    matches!(
        quarry_core::detect_compression(path),
        Ok(Some(_)) | Err(QuarryError::CompressedFile { .. })
    )
}

fn has_gzip_extension(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case(CompressionFormat::Gzip.extension())
    })
}

/// `sales.csv.gz` becomes `sales.csv`; other names are unchanged.
pub(super) fn uncompressed_name(path: &Path) -> PathBuf {
    if has_gzip_extension(path) {
        CompressionFormat::Gzip.suggested_uncompressed_path(path)
    } else {
        path.to_path_buf()
    }
}

/// Quarry never writes gzip, so an output name must not claim it.
pub(super) fn gzip_destination_warning(path: &Path) -> Option<AppMessage> {
    has_gzip_extension(path).then(|| {
        AppMessage::warning(
            "Quarry writes uncompressed delimited files. Choose a file name without .gz.",
        )
    })
}

/// Footer progress for decompression, which runs before any other operation.
pub(super) fn decompression_display(document: &Document) -> Option<ActiveJobDisplay> {
    let (label, fraction, cancel_enabled) = match &document.compressed.as_ref()?.phase {
        Phase::Pending(_) => ("Starting decompression…".to_owned(), 0.0, false),
        Phase::Decompressing { progress, .. } => {
            let fraction = progress_fraction(
                progress.compressed_bytes_read,
                progress.compressed_total_bytes,
                progress.done,
            );
            (
                format!(
                    "Decompressing · {:.1}% · {} · {} rows",
                    fraction * 100.0,
                    format_bytes(progress.decompressed_bytes_written),
                    group_digits(progress.rows_scanned)
                ),
                fraction,
                !progress.done,
            )
        }
        Phase::Complete | Phase::Preview => return None,
    };
    Some(ActiveJobDisplay {
        label,
        fraction,
        animate: false,
        cancel_action: Action::Cancel,
        cancel_label: "Cancel",
        cancel_enabled,
    })
}

impl Document {
    /// Shows the bootstrap rows at once; `start_indexing` then starts decompression.
    pub(super) fn prepare_compressed(
        path: &Path,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Result<Self, String> {
        let bootstrap = CompressedBootstrap::open(path, options, encoding, None)
            .map_err(|error| error.to_string())?;
        let mut document = Self::from_session(bootstrap.preview_session(), path);
        document.compressed = Some(CompressedDocument {
            format: bootstrap.format(),
            compressed_size: bootstrap.compressed_size(),
            phase: Phase::Pending(Box::new(bootstrap)),
        });
        Ok(document)
    }

    pub(super) fn is_compressed(&self) -> bool {
        self.compressed.is_some()
    }

    /// True until the background pass has finished or stopped.
    pub(super) fn decompressing(&self) -> bool {
        self.compressed.as_ref().is_some_and(|compressed| {
            matches!(
                compressed.phase,
                Phase::Pending(_) | Phase::Decompressing { .. }
            )
        })
    }

    /// Why a command that needs the whole file cannot run yet.
    pub(super) fn compressed_unavailable_reason(&self) -> Option<&'static str> {
        match self.compressed.as_ref()?.phase {
            Phase::Pending(_) | Phase::Decompressing { .. } => Some(DECOMPRESSING),
            Phase::Preview => Some(PREVIEW_ONLY),
            Phase::Complete => None,
        }
    }

    /// Every change to a compressed document is unavailable.
    pub(super) fn compressed_read_only_reason(&self) -> Option<&'static str> {
        self.is_compressed().then_some(READ_ONLY)
    }

    pub(super) fn compressed_join_reason(&self) -> Option<&'static str> {
        self.is_compressed().then_some(JOIN_READ_ONLY)
    }

    /// A complete compressed document can be saved as an uncompressed copy without edits.
    pub(super) fn compressed_save_as_ready(&self) -> bool {
        self.compressed
            .as_ref()
            .is_some_and(|compressed| matches!(compressed.phase, Phase::Complete))
    }

    /// Footer text such as `Gzip (read-only) · 12.0 MB → 48.3 MB`.
    pub(super) fn compressed_label(&self) -> Option<String> {
        let compressed = self.compressed.as_ref()?;
        let name = match compressed.format {
            CompressionFormat::Gzip => "Gzip",
        };
        let expanded = match &compressed.phase {
            Phase::Complete => format_bytes(self.session.file_size),
            Phase::Pending(_) => "decompressing".into(),
            Phase::Decompressing { progress, .. } => format!(
                "{} so far",
                format_bytes(progress.decompressed_bytes_written)
            ),
            Phase::Preview => "preview only".into(),
        };
        Some(format!(
            "{name} (read-only) · {} → {expanded}",
            format_bytes(compressed.compressed_size)
        ))
    }

    pub(super) fn compressed_index_status(&self) -> Option<&'static str> {
        match self.compressed.as_ref()?.phase {
            Phase::Pending(_) | Phase::Decompressing { .. } => Some("Decompressing"),
            Phase::Preview => Some("Preview only"),
            Phase::Complete => None,
        }
    }

    /// Starts the single pass that writes the private copy and its index. Returns false
    /// when this document is not waiting to decompress.
    pub(super) fn start_decompression(&mut self) -> Result<bool, String> {
        let Some(compressed) = self.compressed.as_mut() else {
            return Ok(false);
        };
        if !matches!(compressed.phase, Phase::Pending(_)) {
            return Ok(false);
        }
        let Phase::Pending(bootstrap) = std::mem::replace(&mut compressed.phase, Phase::Preview)
        else {
            unreachable!("the phase was checked above");
        };
        let config = CompressedOpenConfig::new(self.working_directory.clone());
        match bootstrap.start_decompression(config, self.working_cleanup.clone()) {
            Ok(job) => {
                let progress = job.progress();
                compressed.phase = Phase::Decompressing { job, progress };
                self.last_poll = Instant::now() - POLL_INTERVAL;
                Ok(true)
            }
            Err(error) => Err(self.compressed_failure(error)),
        }
    }

    /// Follows the background pass. Returns true while decompression owns this document.
    pub(super) fn poll_decompression(&mut self) -> Result<bool, String> {
        let Some(Phase::Decompressing { job, progress }) = self
            .compressed
            .as_mut()
            .map(|compressed| &mut compressed.phase)
        else {
            return Ok(false);
        };
        let current = job.progress();
        if self.last_poll.elapsed() < POLL_INTERVAL && !current.done {
            return Ok(true);
        }
        *progress = current;
        self.last_poll = Instant::now();
        if current.done {
            self.finish_decompression()?;
            return Ok(true);
        }
        self.progress.rows_scanned = current.rows_scanned;
        self.progress.bytes_scanned = current.decompressed_bytes_written;
        self.progress.columns_found = current.columns_found;
        self.progress.elapsed = current.elapsed;
        self.apply_indexed_width(current.columns_found);
        self.load_buffer(self.viewport_start)?;
        Ok(true)
    }

    /// Reads rows that the running pass has already written and indexed.
    pub(super) fn read_decompressed_rows(
        &self,
        start: u64,
        count: usize,
    ) -> Option<Result<Vec<Row>, QuarryError>> {
        match &self.compressed.as_ref()?.phase {
            Phase::Decompressing { job, .. } => Some(job.read_rows(start, count)),
            _ => None,
        }
    }

    /// Stops decompression at the user's request. The worker removes its private copy
    /// before this returns. None when no decompression is running.
    pub(super) fn cancel_decompression(&mut self) -> Option<Result<&'static str, String>> {
        let Some(Phase::Decompressing { job, .. }) =
            self.compressed.as_ref().map(|compressed| &compressed.phase)
        else {
            return None;
        };
        job.cancel();
        Some(
            self.finish_decompression().map(|notice| {
                notice.unwrap_or("Decompression finished before it could be cancelled.")
            }),
        )
    }

    /// Abandons decompression when the document closes; dropping the job removes its copy.
    pub(super) fn stop_decompression(&mut self) {
        if let Some(compressed) = self.compressed.as_mut()
            && matches!(
                compressed.phase,
                Phase::Pending(_) | Phase::Decompressing { .. }
            )
            && let Phase::Decompressing { job, .. } =
                std::mem::replace(&mut compressed.phase, Phase::Preview)
        {
            job.cancel();
            drop(job);
        }
    }

    /// Applies a format change by re-reading the verified private copy instead of
    /// decompressing again. None when the compressed source must be read again.
    pub(super) fn reopen_decompressed_copy(
        &self,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Option<Self> {
        let compressed = self
            .compressed
            .as_ref()
            .filter(|compressed| matches!(compressed.phase, Phase::Complete))?;
        let options = OpenOptions {
            rows: BOOTSTRAP_ROWS + 2 * OVERSCAN_ROWS + 1,
            ..options
        };
        let session = self
            .session
            .reopen_compressed_backing(options, encoding)
            .ok()?;
        let mut document = Self::from_session(session, &self.logical_path);
        document.compressed = Some(CompressedDocument {
            format: compressed.format,
            compressed_size: compressed.compressed_size,
            phase: Phase::Complete,
        });
        Some(document)
    }

    /// Collects the finished pass: adopt the indexed copy, or fall back to the preview.
    fn finish_decompression(&mut self) -> Result<Option<&'static str>, String> {
        let Some(compressed) = self.compressed.as_mut() else {
            return Ok(None);
        };
        if !matches!(compressed.phase, Phase::Decompressing { .. }) {
            return Ok(None);
        }
        let Phase::Decompressing { job, .. } =
            std::mem::replace(&mut compressed.phase, Phase::Preview)
        else {
            unreachable!("the phase was checked above");
        };
        match job.wait() {
            Ok(CompressedOpenOutcome::Complete { session, index }) => {
                self.adopt_decompressed(session, index)?;
                Ok(None)
            }
            Ok(CompressedOpenOutcome::Cancelled) => {
                self.show_preview()?;
                Ok(Some(CANCELLED))
            }
            Err(error) => {
                let message = format!("{} {FAILED_PREVIEW}", self.compressed_failure(error));
                self.show_preview()?;
                Err(message)
            }
        }
    }

    /// Switches to the verified copy and its complete index; nothing is scanned again.
    fn adopt_decompressed(
        &mut self,
        session: Session,
        index: StructuralIndex,
    ) -> Result<(), String> {
        let width = index.maximum_width();
        self.session = Arc::new(session);
        self.progress = IndexProgress {
            bytes_scanned: index.indexed_bytes(),
            rows_scanned: index.indexed_rows(),
            columns_found: width,
            file_size: self.session.file_size,
            elapsed: self.progress.elapsed,
            done: true,
            cancelled: false,
        };
        self.index = Some(index);
        if let Some(compressed) = self.compressed.as_mut() {
            compressed.phase = Phase::Complete;
        }
        self.apply_indexed_width(width);
        self.refresh_column_headers();
        // Rows read during decompression match the copy; reload them from its index.
        self.buffered_rows.clear();
        self.load_buffer(self.viewport_start)
    }

    /// Leaves only the bounded bootstrap rows; rows read from the removed copy go too.
    fn show_preview(&mut self) -> Result<(), String> {
        self.index = None;
        self.progress.rows_scanned = 0;
        self.progress.bytes_scanned = 0;
        self.progress.done = true;
        self.progress.cancelled = true;
        self.selection = None;
        self.selected_rows.clear();
        let last_start = max_viewport_start(self.available_rows(), self.visible_rows);
        self.viewport_start = self.viewport_start.min(last_start.max(self.data_start));
        self.buffer_start = self.data_start;
        self.buffered_rows.clear();
        self.whitespace.invalidate_rows();
        self.load_buffer(self.viewport_start)
    }

    /// Keeps the typed diagnostic and names the fix for a full temporary folder.
    fn compressed_failure(&mut self, error: QuarryError) -> String {
        let storage = matches!(error, QuarryError::InsufficientStorage { .. });
        let mut message = self.diagnostic_error(error);
        if storage {
            message.push_str(
                " Choose another location in File > Temporary Storage…, then use File > Reload from Disk.",
            );
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
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
        assert!(gzip_destination_warning(Path::new("/out/copy.csv.GZ")).is_some());
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
    fn cancelling_keeps_preview_rows_and_removes_the_private_copy() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let source = directory.path().join("large.csv.gz");
        large_gzip(&source, 256, &[]);
        let (mut app, ctx) = app_with(&source, scratch.path());
        let first_rows = active(&mut app).session.first_rows.len() as u64;
        assert_eq!(leftovers(scratch.path()).len(), 1);

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

        // Reload reads the compressed source again.
        app.reload_document();
        assert!(active(&mut app).decompressing());
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
            document.start_delete_rows(vec![1..=1]).unwrap_err().text,
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
    fn decompression_shares_the_job_limit_and_closing_removes_its_copy() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..3)
            .map(|index| {
                let path = directory.path().join(format!("large-{index}.csv.gz"));
                large_gzip(&path, 256, &[]);
                path
            })
            .collect();
        let (mut app, ctx) = app_with(&paths[0], scratch.path());
        app.open_path(paths[1].clone()).unwrap();
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
}
