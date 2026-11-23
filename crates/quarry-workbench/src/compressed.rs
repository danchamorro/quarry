//! Read-only documents opened from gzip-compressed files.
//!
//! The first rows come from a bounded in-memory bootstrap and show at once. One
//! background pass then writes a private decompressed copy and indexes it; the
//! document adopts that session and index when the pass ends, with no second scan.
//! Cancelling, or a failure, removes the copy and leaves only the bootstrap rows as a
//! read-only preview. Every mutating command is unavailable. Save As writes an
//! uncompressed copy, which then opens as an ordinary, editable document.

use crate::{
    AppMessage, BOOTSTRAP_ROWS, Document, OVERSCAN_ROWS, POLL_INTERVAL, format_bytes,
    format_unavailable_scratch_notice, group_digits, max_viewport_start, progress_fraction,
};
use quarry_core::{
    CompressedBootstrap, CompressedDecompressJob, CompressedOpenConfig, CompressedOpenOutcome,
    CompressedProgress, CompressionFormat, Encoding, IndexProgress, OpenOptions, QuarryError, Row,
    Session, StructuralIndex,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

pub const READ_ONLY: &str =
    "Compressed files open read-only. Use Save As… to create an editable uncompressed copy.";
pub const DECOMPRESSING: &str = "Wait for decompression to finish before using this command.";
pub const PREVIEW_ONLY: &str = "Only the first rows of this compressed file are loaded. Use File > Reload from Disk to load the full file.";
/// Reloading the same file would fail again, so this names Inspect File instead.
pub const UNREADABLE_PREVIEW: &str = "Only the first rows of this compressed file are loaded because later rows are not valid delimited text. Choose File > Inspect File… to locate the problem.";
pub const WRITE_SLOTS_FULL: &str =
    "Wait for an active file operation to finish before opening another compressed file.";
pub const CANCELLED: &str = "Decompression cancelled; temporary files were removed. Showing initial preview rows. Use File > Reload from Disk to load the full file.";
pub const JOIN_READ_ONLY: &str = "Compressed files cannot be joined directly. Use Save As… to create an uncompressed copy, then join that copy.";
pub const FAILED_PREVIEW: &str =
    "Showing the initial preview rows only; no temporary files were kept.";
pub const FAILED_TEXT_PREVIEW: &str = "Showing the initial preview rows only; no temporary files were kept. Choose Details / Inspect… to locate the malformed text.";

/// Counts running decompression workers, including a closed tab's worker that its
/// cleanup is still stopping, so the shared file-operation limit holds until every
/// worker has exited.
#[derive(Clone, Default)]
pub struct DecompressionSlots {
    active: Arc<AtomicUsize>,
    #[cfg(any(test, feature = "test-hooks"))]
    peak: Arc<AtomicUsize>,
}

impl DecompressionSlots {
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// The most workers that ever ran at once; read by debug-build tests that hold
    /// workers at a checkpoint.
    #[cfg(all(any(test, feature = "test-hooks"), debug_assertions))]
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }

    /// Claims a slot for a worker about to start; it is freed when the slot drops.
    pub fn claim(&self) -> DecompressionSlot {
        let running = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        #[cfg(any(test, feature = "test-hooks"))]
        self.peak.fetch_max(running, Ordering::AcqRel);
        #[cfg(not(any(test, feature = "test-hooks")))]
        let _ = running;
        DecompressionSlot(Arc::clone(&self.active))
    }
}

/// Held beside a decompression job and dropped after it, once its worker has exited.
pub struct DecompressionSlot(Arc<AtomicUsize>);

impl Drop for DecompressionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Why a file could not be opened, and whether Inspect File can examine it.
pub struct OpenFailure {
    pub message: String,
    pub inspectable: bool,
}

/// True when valid gzip held text that could not be read. Inspect File then reads a
/// raw private copy to locate the problem. Damaged or unsupported compressed data,
/// size limits and storage failures are reported as they are.
fn text_failure(error: &QuarryError) -> bool {
    matches!(
        error,
        QuarryError::Parse(_)
            | QuarryError::Encoding(_)
            | QuarryError::RecordEncoding { .. }
            | QuarryError::EncodingAt { .. }
            | QuarryError::BootstrapLimitExceeded { .. }
            | QuarryError::RecordTooLarge { .. }
    )
}

/// The compressed origin of a read-only document.
pub struct CompressedDocument {
    format: CompressionFormat,
    compressed_size: u64,
    phase: Phase,
    /// The background pass stopped at text it could not read, so only a raw copy
    /// of the whole file can show where.
    unreadable_text: bool,
}

enum Phase {
    /// Bootstrap rows are visible; decompression starts once the tab has its storage settings.
    Pending(Box<CompressedBootstrap>),
    Decompressing {
        job: CompressedDecompressJob,
        progress: CompressedProgress,
        /// Declared after the job, so it is freed only once the worker has exited.
        slot: DecompressionSlot,
    },
    /// The session reads the verified private copy.
    Complete,
    /// Cancelled or failed: only the bootstrap rows remain, and no private copy is kept.
    Preview,
}

/// Dormancy policy enforced for a gzip-compressed document's lifecycle phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressedDormancyPolicy {
    /// Storage review / decompression start has not run yet; cannot go dormant.
    PinnedPendingStorage,
    /// Decompression worker is actively running; pinned until completion or cancellation.
    PinnedDecompressing,
    /// Decompression finished; can go dormant while retaining the private decompressed copy.
    DormantKeepPrivateCopy,
    /// Cancelled or failed preview; can go dormant and must restore preview without restarting decompression.
    DormantPreviewOnly,
}

impl CompressedDocument {
    /// Returns the dormancy policy required by the current compressed lifecycle phase.
    pub fn dormancy_policy(&self) -> CompressedDormancyPolicy {
        match &self.phase {
            Phase::Pending(_) => CompressedDormancyPolicy::PinnedPendingStorage,
            Phase::Decompressing { .. } => CompressedDormancyPolicy::PinnedDecompressing,
            Phase::Complete => CompressedDormancyPolicy::DormantKeepPrivateCopy,
            Phase::Preview => CompressedDormancyPolicy::DormantPreviewOnly,
        }
    }
}

/// Frontend-neutral progress snapshot for an active decompression pass.
#[derive(Debug, Clone, PartialEq)]
pub struct DecompressionStatus {
    pub label: String,
    pub fraction: f32,
    pub cancel_enabled: bool,
}

/// True for gzip data. Unsupported or damaged compressed files take the ordinary open
/// path, whose error names the problem.
pub fn is_gzip(path: &Path) -> bool {
    matches!(
        quarry_core::detect_compression(path),
        Ok(Some(CompressionFormat::Gzip))
    )
}

/// True when a failed open concerns compressed data, which raw inspection cannot read.
pub fn compressed_source(path: &Path) -> bool {
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
pub fn uncompressed_name(path: &Path) -> PathBuf {
    if has_gzip_extension(path) {
        CompressionFormat::Gzip.suggested_uncompressed_path(path)
    } else {
        path.to_path_buf()
    }
}

pub const COMPRESSED_DESTINATION: &str =
    "Quarry writes uncompressed delimited files. Choose a file name without .gz or .tgz.";

/// Quarry never writes gzip, so an output name must not claim it. This is the core
/// rule every writer enforces, checked early for a clear message.
pub fn gzip_destination_warning(path: &Path) -> Option<AppMessage> {
    quarry_core::is_compressed_destination(path)
        .then(|| AppMessage::warning(COMPRESSED_DESTINATION))
}

/// Footer progress for decompression, which runs before any other operation.
pub fn decompression_status(document: &Document) -> Option<DecompressionStatus> {
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
    Some(DecompressionStatus {
        label,
        fraction,
        cancel_enabled,
    })
}

impl Document {
    /// Shows the bootstrap rows at once; `start_indexing` then starts decompression.
    pub(crate) fn prepare_compressed(
        path: &Path,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Result<Self, OpenFailure> {
        let bootstrap =
            CompressedBootstrap::open(path, options, encoding, None).map_err(|error| {
                OpenFailure {
                    inspectable: text_failure(&error),
                    message: error.to_string(),
                }
            })?;
        let mut document = Self::from_session(bootstrap.preview_session(), path);
        document.compressed = Some(CompressedDocument {
            format: bootstrap.format(),
            compressed_size: bootstrap.compressed_size(),
            phase: Phase::Pending(Box::new(bootstrap)),
            unreadable_text: false,
        });
        Ok(document)
    }

    pub fn is_compressed(&self) -> bool {
        self.compressed.is_some()
    }

    /// True for a prepared compressed document whose pass has not started yet.
    pub fn awaiting_decompression(&self) -> bool {
        self.compressed
            .as_ref()
            .is_some_and(|compressed| matches!(compressed.phase, Phase::Pending(_)))
    }

    /// True while a background worker runs and holds a decompression slot.
    pub fn decompression_running(&self) -> bool {
        self.compressed
            .as_ref()
            .is_some_and(|compressed| matches!(compressed.phase, Phase::Decompressing { .. }))
    }

    /// True until the background pass has finished or stopped.
    pub fn decompressing(&self) -> bool {
        self.compressed.as_ref().is_some_and(|compressed| {
            matches!(
                compressed.phase,
                Phase::Pending(_) | Phase::Decompressing { .. }
            )
        })
    }

    /// Why a command that needs the whole file cannot run yet.
    pub fn compressed_unavailable_reason(&self) -> Option<&'static str> {
        let compressed = self.compressed.as_ref()?;
        match compressed.phase {
            Phase::Pending(_) | Phase::Decompressing { .. } => Some(DECOMPRESSING),
            Phase::Preview if compressed.unreadable_text => Some(UNREADABLE_PREVIEW),
            Phase::Preview => Some(PREVIEW_ONLY),
            Phase::Complete => None,
        }
    }

    /// The gzip file that Inspect File reads through a raw copy, when the background
    /// pass stopped at text it could not read and only the preview rows remain.
    pub fn raw_inspection_target(&self) -> Option<&Path> {
        self.compressed
            .as_ref()
            .filter(|compressed| {
                compressed.unreadable_text && matches!(compressed.phase, Phase::Preview)
            })
            .map(|_| self.logical_path.as_path())
    }

    /// Every change to a compressed document is unavailable.
    pub fn compressed_read_only_reason(&self) -> Option<&'static str> {
        self.is_compressed().then_some(READ_ONLY)
    }

    pub fn compressed_join_reason(&self) -> Option<&'static str> {
        self.is_compressed().then_some(JOIN_READ_ONLY)
    }

    /// A complete compressed document can be saved as an uncompressed copy without edits.
    pub fn compressed_save_as_ready(&self) -> bool {
        self.compressed
            .as_ref()
            .is_some_and(|compressed| matches!(compressed.phase, Phase::Complete))
    }

    /// Footer text such as `Gzip (read-only) · 12.0 MB → 48.3 MB`.
    pub fn compressed_label(&self) -> Option<String> {
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

    pub fn compressed_index_status(&self) -> Option<&'static str> {
        match self.compressed.as_ref()?.phase {
            Phase::Pending(_) | Phase::Decompressing { .. } => Some("Decompressing"),
            Phase::Preview => Some("Preview only"),
            Phase::Complete => None,
        }
    }

    /// Starts the single pass that writes the private copy and its index. Returns false
    /// when this document is not waiting to decompress.
    pub fn start_decompression(&mut self) -> Result<bool, String> {
        let Some(compressed) = self.compressed.as_mut() else {
            return Ok(false);
        };
        if !matches!(compressed.phase, Phase::Pending(_)) {
            return Ok(false);
        }
        if self.custom_working_directory
            && let Err(error) = quarry_core::validate_scratch_directory(&self.working_directory)
        {
            return Err(format_unavailable_scratch_notice(
                &self.working_directory,
                &error.to_string(),
            ));
        }
        let Phase::Pending(bootstrap) = std::mem::replace(&mut compressed.phase, Phase::Preview)
        else {
            unreachable!("the phase was checked above");
        };
        let config = CompressedOpenConfig::new(self.working_directory.clone());
        let slot = self.decompression_slots.claim();
        match bootstrap.start_decompression(config, self.working_cleanup.clone()) {
            Ok(job) => {
                let progress = job.progress();
                compressed.phase = Phase::Decompressing {
                    job,
                    progress,
                    slot,
                };
                self.last_poll = Instant::now() - POLL_INTERVAL;
                Ok(true)
            }
            Err(error) => Err(self.compressed_failure(error)),
        }
    }

    /// Follows the background pass. Returns true while decompression owns this document.
    pub fn poll_decompression(&mut self) -> Result<bool, String> {
        let Some(Phase::Decompressing { job, progress, .. }) = self
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
    pub fn read_decompressed_rows(
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
    pub fn cancel_decompression(&mut self) -> Option<Result<&'static str, String>> {
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

    /// Stops a running pass and waits for its worker before a replacement starts, so
    /// its slot is free when this returns. The document keeps the bootstrap preview,
    /// or the verified copy if the pass finished first, and stays usable.
    pub fn release_decompression(&mut self) {
        // The outcome is already reflected in the document's phase and rows.
        let _ = self.cancel_decompression();
    }

    /// Abandons decompression when the document closes; dropping the job removes its copy.
    pub fn stop_decompression(&mut self) {
        if let Some(compressed) = self.compressed.as_mut()
            && matches!(
                compressed.phase,
                Phase::Pending(_) | Phase::Decompressing { .. }
            )
            && let Phase::Decompressing { job, slot, .. } =
                std::mem::replace(&mut compressed.phase, Phase::Preview)
        {
            job.cancel();
            drop(job);
            drop(slot);
        }
    }

    /// Applies a format change by re-reading the verified private copy instead of
    /// decompressing again. None when the compressed source must be read again.
    pub fn reopen_decompressed_copy(
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
            unreadable_text: false,
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
        let Phase::Decompressing { job, slot, .. } =
            std::mem::replace(&mut compressed.phase, Phase::Preview)
        else {
            unreachable!("the phase was checked above");
        };
        let outcome = job.wait();
        drop(slot);
        match outcome {
            Ok(CompressedOpenOutcome::Complete { session, index }) => {
                self.adopt_decompressed(session, index)?;
                Ok(None)
            }
            Ok(CompressedOpenOutcome::Cancelled) => {
                self.show_preview()?;
                Ok(Some(CANCELLED))
            }
            Err(error) => {
                let unreadable_text = text_failure(&error);
                let message = format!(
                    "{} {}",
                    self.compressed_failure(error),
                    if unreadable_text {
                        FAILED_TEXT_PREVIEW
                    } else {
                        FAILED_PREVIEW
                    }
                );
                if let Some(compressed) = self.compressed.as_mut() {
                    compressed.unreadable_text = unreadable_text;
                }
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
        self.cursor.clear();
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
