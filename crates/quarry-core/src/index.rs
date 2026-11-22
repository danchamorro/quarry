use std::collections::BTreeMap;
use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use memchr::memchr;
use quarry_delimited::{
    ByteEncodingClass, ParseError, ParseErrorKind, ScannerDialect, SimdBackend, StructuralScanner,
    active_simd_backend,
};

use crate::Encoding;
use crate::encoding::EncodedScanner;
use crate::export::source_matches_stamp;
use crate::pool::{JobExecutor, JobPriority, engine_pool, resolve_worker_count};
use crate::positional_io::{advise_readahead, read_at_exact_or_eof};
use crate::source_stamp::open_regular_file;
use crate::{CompletedRecordCount, QuarryError, Session, SourceStamp};

const DEFAULT_INDEX_CHUNK_BYTES: usize = 1024 * 1024;
const DEFAULT_CHECKPOINT_EVERY: u64 = 4096;
const DEFAULT_INDEX_MEMORY_BUDGET: usize = 16 * 1024 * 1024;
const DEFAULT_CHECKPOINT_BYTE_STRIDE: u64 = 256 * 1024;
const PARALLEL_INDEX_MIN_BYTES: u64 = 4 * 1024 * 1024;
const PARALLEL_MIN_CHUNK_BYTES: usize = 64 * 1024;
const PARALLEL_MAX_CHUNK_BYTES: usize = 2 * 1024 * 1024;
const ALIGNMENT_PROBE_BYTES: usize = 64 * 1024;
const MAX_ALIGNMENT_SEARCH_BYTES: u64 = 256 * 1024;
const WORKER_CANCEL_SLICE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct IndexConfig {
    pub chunk_bytes: usize,
    pub checkpoint_every: u64,
    pub memory_budget_bytes: usize,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            chunk_bytes: DEFAULT_INDEX_CHUNK_BYTES,
            checkpoint_every: DEFAULT_CHECKPOINT_EVERY,
            memory_budget_bytes: DEFAULT_INDEX_MEMORY_BUDGET,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    pub row: u64,
    pub offset: u64,
}

/// Runtime telemetry reported by structural indexing jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexTelemetry {
    pub effective_workers: usize,
    pub simd_backend: SimdBackend,
    pub chunks_processed: u64,
    pub dual_scan_chunks: u64,
    pub quoted_entry_chunks: u64,
    pub rescanned_bytes: u64,
    pub byte_stride_checkpoints: bool,
}

impl Default for IndexTelemetry {
    fn default() -> Self {
        Self {
            effective_workers: 1,
            simd_backend: active_simd_backend(),
            chunks_processed: 0,
            dual_scan_chunks: 0,
            quoted_entry_chunks: 0,
            rescanned_bytes: 0,
            byte_stride_checkpoints: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StructuralIndex {
    checkpoints: Vec<Checkpoint>,
    checkpoint_every: u64,
    checkpoint_byte_stride: Option<u64>,
    max_checkpoints: usize,
    indexed_rows: u64,
    maximum_width: usize,
    indexed_bytes: u64,
    telemetry: IndexTelemetry,
}

impl PartialEq for StructuralIndex {
    fn eq(&self, other: &Self) -> bool {
        self.checkpoints == other.checkpoints
            && self.checkpoint_every == other.checkpoint_every
            && self.checkpoint_byte_stride == other.checkpoint_byte_stride
            && self.max_checkpoints == other.max_checkpoints
            && self.indexed_rows == other.indexed_rows
            && self.maximum_width == other.maximum_width
            && self.indexed_bytes == other.indexed_bytes
    }
}

impl Eq for StructuralIndex {}

impl StructuralIndex {
    pub(crate) fn new(config: IndexConfig) -> Result<Self, QuarryError> {
        if config.chunk_bytes == 0 || config.checkpoint_every == 0 {
            return Err(QuarryError::InvalidOption(
                "index chunk and checkpoint interval must be non-zero",
            ));
        }
        let checkpoint_capacity = config.memory_budget_bytes / std::mem::size_of::<Checkpoint>();
        if checkpoint_capacity < 2 {
            return Err(QuarryError::InvalidOption(
                "index memory budget must fit at least two checkpoints",
            ));
        }
        let max_checkpoints = 1_usize << checkpoint_capacity.ilog2();
        Ok(Self {
            checkpoints: vec![Checkpoint { row: 0, offset: 0 }],
            checkpoint_every: config.checkpoint_every,
            checkpoint_byte_stride: None,
            max_checkpoints,
            indexed_rows: 0,
            maximum_width: 0,
            indexed_bytes: 0,
            telemetry: IndexTelemetry::default(),
        })
    }

    pub(crate) fn enable_byte_stride_checkpoints(&mut self, stride: u64) {
        self.checkpoint_byte_stride = Some(stride.max(1));
        self.telemetry.byte_stride_checkpoints = true;
    }

    pub(crate) fn record_boundary(&mut self, offset: u64) {
        self.indexed_rows += 1;
        self.indexed_bytes = offset;
        if self.checkpoint_byte_stride.is_some() {
            let row = self.indexed_rows;
            self.record_byte_stride_checkpoint(row, offset);
            return;
        }
        if self.indexed_rows.is_multiple_of(self.checkpoint_every) {
            if self.checkpoints.len() == self.max_checkpoints {
                self.checkpoint_every = self.checkpoint_every.saturating_mul(2);
                let interval = self.checkpoint_every;
                self.checkpoints
                    .retain(|checkpoint| checkpoint.row.is_multiple_of(interval));
                self.checkpoints.shrink_to_fit();
            }
            if self.indexed_rows.is_multiple_of(self.checkpoint_every) {
                self.checkpoints.push(Checkpoint {
                    row: self.indexed_rows,
                    offset,
                });
            }
        }
    }

    pub(crate) fn record_byte_stride_checkpoint(&mut self, row: u64, offset: u64) {
        let Some(stride) = self.checkpoint_byte_stride else {
            return;
        };
        let bucket = offset / stride;
        let last_bucket = self.checkpoints.last().map_or(0, |cp| cp.offset / stride);
        if bucket <= last_bucket {
            return;
        }
        while self.checkpoints.len() == self.max_checkpoints {
            let current_stride = self.checkpoint_byte_stride.unwrap();
            let new_stride = current_stride.saturating_mul(2);
            if new_stride == current_stride {
                let mut compacted = Vec::with_capacity(self.checkpoints.len() / 2 + 1);
                for (i, &cp) in self.checkpoints.iter().enumerate() {
                    if i % 2 == 0 {
                        compacted.push(cp);
                    }
                }
                compacted.shrink_to_fit();
                self.checkpoints = compacted;
                break;
            }
            self.checkpoint_byte_stride = Some(new_stride);
            let mut compacted = Vec::with_capacity(self.checkpoints.len() / 2 + 2);
            compacted.push(self.checkpoints[0]);
            let mut prev_bucket = 0u64;
            for &cp in &self.checkpoints[1..] {
                let b = cp.offset / new_stride;
                if b > prev_bucket {
                    compacted.push(cp);
                    prev_bucket = b;
                }
            }
            compacted.shrink_to_fit();
            self.checkpoints = compacted;
        }
        let active_stride = self.checkpoint_byte_stride.unwrap();
        let active_bucket = offset / active_stride;
        let active_last_bucket = self
            .checkpoints
            .last()
            .map_or(0, |cp| cp.offset / active_stride);
        if active_bucket > active_last_bucket
            && self
                .checkpoints
                .last()
                .is_none_or(|cp| cp.row < row && cp.offset < offset)
        {
            self.checkpoints.push(Checkpoint { row, offset });
        }
    }

    /// Record bytes scanned so far by an external single-pass writer.
    pub(crate) fn set_scan_progress(&mut self, indexed_bytes: u64, maximum_width: usize) {
        self.indexed_bytes = indexed_bytes;
        self.maximum_width = maximum_width;
    }

    pub(crate) fn finish_output(&mut self, bytes_written: u64, maximum_width: usize) {
        self.maximum_width = maximum_width;
        self.indexed_bytes = bytes_written;
    }

    /// Widest completed logical record in the indexed portion, including the header.
    pub fn maximum_width(&self) -> usize {
        self.maximum_width
    }

    pub fn indexed_rows(&self) -> u64 {
        self.indexed_rows
    }

    pub fn indexed_bytes(&self) -> u64 {
        self.indexed_bytes
    }

    pub fn checkpoint_every(&self) -> u64 {
        self.checkpoint_every
    }

    /// Byte stride between checkpoints when byte-stride indexing is active, or `None` for row-strided indexes.
    pub fn checkpoint_byte_stride(&self) -> Option<u64> {
        self.checkpoint_byte_stride
    }

    /// Runtime indexing telemetry (worker count, active SIMD backend, dual-scan stats).
    pub fn telemetry(&self) -> IndexTelemetry {
        self.telemetry
    }

    pub fn memory_bytes(&self) -> usize {
        self.checkpoints.capacity() * std::mem::size_of::<Checkpoint>()
    }

    pub fn checkpoints(&self) -> &[Checkpoint] {
        &self.checkpoints
    }

    pub fn nearest_checkpoint(&self, row: u64) -> Checkpoint {
        let position = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.row <= row)
            .saturating_sub(1);
        self.checkpoints[position]
    }
}

/// A completed index tied to the exact file that an operation published.
/// Its provenance can only be created by the core's guarded output writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedIndex {
    index: Box<StructuralIndex>,
    source_stamp: Box<SourceStamp>,
    delimiter: u8,
    encoding: Encoding,
}

impl CompletedIndex {
    pub(crate) fn from_output(
        index: StructuralIndex,
        source_stamp: SourceStamp,
        delimiter: u8,
        encoding: Encoding,
    ) -> Option<Self> {
        (index.indexed_bytes == source_stamp.file_size()).then_some(Self {
            index: Box::new(index),
            source_stamp: Box::new(source_stamp),
            delimiter,
            encoding,
        })
    }
}

impl Session {
    /// Reuse an operation's index only while its published file is still current.
    pub fn adopt_completed_index(
        &self,
        completed: CompletedIndex,
    ) -> Result<StructuralIndex, QuarryError> {
        if completed.delimiter != self.dialect.delimiter || completed.encoding != self.encoding {
            return Err(QuarryError::InvalidOption(
                "completed index format differs from the open document",
            ));
        }
        if completed.index.indexed_bytes != self.file_size
            || !self.source_stamp.matches(&completed.source_stamp)
        {
            return Err(QuarryError::SourceChanged);
        }
        self.ensure_source_unchanged()?;
        *self.completed_record_count.lock().unwrap() = Some(CompletedRecordCount {
            delimiter: completed.delimiter,
            encoding: completed.encoding,
            file_size: completed.index.indexed_bytes,
            records: completed.index.indexed_rows,
        });
        Ok(*completed.index)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct IndexProgress {
    pub bytes_scanned: u64,
    pub rows_scanned: u64,
    pub columns_found: usize,
    pub file_size: u64,
    pub elapsed: Duration,
    pub done: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy)]
struct ParallelTuning {
    requested_threads: usize,
    min_parallel_bytes: u64,
    byte_stride: u64,
    max_alignment_search: u64,
    allow_small_chunks: bool,
}

impl ParallelTuning {
    fn for_production(requested_threads: usize) -> Self {
        Self {
            requested_threads,
            min_parallel_bytes: PARALLEL_INDEX_MIN_BYTES,
            byte_stride: DEFAULT_CHECKPOINT_BYTE_STRIDE,
            max_alignment_search: MAX_ALIGNMENT_SEARCH_BYTES,
            allow_small_chunks: false,
        }
    }
}

struct SharedState {
    index: RwLock<StructuralIndex>,
    bytes_scanned: AtomicU64,
    rows_scanned: AtomicU64,
    columns_found: AtomicUsize,
    finished_nanos: AtomicU64,
    done: AtomicBool,
    cancelled: AtomicBool,
    error: Mutex<Option<String>>,
    started: Instant,
    file_size: u64,
}

struct WorkerCompletion<'a>(&'a SharedState);

impl Drop for WorkerCompletion<'_> {
    fn drop(&mut self) {
        let elapsed = self.0.started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.0
            .finished_nanos
            .store(elapsed.max(1), Ordering::Release);
        self.0.done.store(true, Ordering::Release);
    }
}

pub struct IndexJob {
    shared: Arc<SharedState>,
    handle: Option<JoinHandle<Result<(), QuarryError>>>,
}

impl IndexJob {
    pub(crate) fn start(
        session: &Session,
        config: IndexConfig,
        threads: Option<usize>,
    ) -> Result<Self, QuarryError> {
        let requested_threads = resolve_worker_count(threads)?;
        Self::start_with_tuning(
            session,
            config,
            ParallelTuning::for_production(requested_threads),
        )
    }

    fn start_with_tuning(
        session: &Session,
        config: IndexConfig,
        tuning: ParallelTuning,
    ) -> Result<Self, QuarryError> {
        let index = StructuralIndex::new(config)?;
        let path = session.path.clone();
        let file_size = session.file_size;
        let delimiter = session.dialect.delimiter;
        let encoding = session.encoding;
        let source_stamp = session.source_stamp.clone();
        let completed_record_count = Arc::clone(&session.completed_record_count);
        let file = open_regular_file(&path)?;
        if !source_matches_stamp(&file, &path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        let shared = Arc::new(SharedState {
            index: RwLock::new(index),
            bytes_scanned: AtomicU64::new(0),
            rows_scanned: AtomicU64::new(0),
            columns_found: AtomicUsize::new(0),
            finished_nanos: AtomicU64::new(0),
            done: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            error: Mutex::new(None),
            started: Instant::now(),
            file_size,
        });
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-index".into())
            .spawn(move || {
                let _completion = WorkerCompletion(&worker_state);
                let result = run_indexer(
                    file,
                    file_size,
                    delimiter,
                    encoding,
                    config,
                    tuning,
                    &worker_state,
                )
                .and_then(|file| {
                    if !source_matches_stamp(&file, &path, &source_stamp)? {
                        return Err(QuarryError::SourceChanged);
                    }
                    if !worker_state.cancelled.load(Ordering::Acquire)
                        && worker_state.bytes_scanned.load(Ordering::Acquire)
                            == source_stamp.file_size()
                    {
                        *completed_record_count.lock().unwrap() = Some(CompletedRecordCount {
                            delimiter,
                            encoding,
                            file_size: source_stamp.file_size(),
                            records: worker_state.index.read().unwrap().indexed_rows,
                        });
                    }
                    Ok(())
                });
                if let Err(error) = &result {
                    *worker_state.error.lock().unwrap() = Some(error.to_string());
                }
                result
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> IndexProgress {
        let done = self.shared.done.load(Ordering::Acquire);
        let finished_nanos = self.shared.finished_nanos.load(Ordering::Acquire);
        IndexProgress {
            bytes_scanned: self.shared.bytes_scanned.load(Ordering::Acquire),
            rows_scanned: self.shared.rows_scanned.load(Ordering::Acquire),
            columns_found: self.shared.columns_found.load(Ordering::Acquire),
            file_size: self.shared.file_size,
            elapsed: if finished_nanos == 0 {
                self.shared.started.elapsed()
            } else {
                Duration::from_nanos(finished_nanos)
            },
            done,
            cancelled: self.shared.cancelled.load(Ordering::Acquire),
        }
    }

    pub fn snapshot(&self) -> StructuralIndex {
        self.shared.index.read().unwrap().clone()
    }

    pub fn telemetry(&self) -> IndexTelemetry {
        self.shared.index.read().unwrap().telemetry
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
    }

    pub fn wait(mut self) -> Result<StructuralIndex, QuarryError> {
        let result = self
            .handle
            .take()
            .expect("index handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?;
        result?;
        let index = self.shared.index.read().unwrap().clone();
        Ok(index)
    }
}

impl Drop for IndexJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

fn run_indexer(
    file: File,
    file_size: u64,
    delimiter: u8,
    encoding: Encoding,
    config: IndexConfig,
    tuning: ParallelTuning,
    shared: &SharedState,
) -> Result<File, QuarryError> {
    let chunk_ok = tuning.allow_small_chunks || config.chunk_bytes >= PARALLEL_MIN_CHUNK_BYTES;
    let use_byte_stride = config.checkpoint_every == DEFAULT_CHECKPOINT_EVERY
        && file_size >= tuning.min_parallel_bytes
        && chunk_ok;

    if let Some(byte_class) = encoding.simd_byte_class()
        && use_byte_stride
        && tuning.requested_threads > 1
    {
        let file = Arc::new(file);
        run_parallel_indexer(
            Arc::clone(&file),
            file_size,
            delimiter,
            byte_class,
            config,
            tuning,
            shared,
        )?;
        let file = Arc::try_unwrap(file).unwrap_or_else(|arc| {
            arc.try_clone()
                .expect("file handle clone after worker drain")
        });
        return Ok(file);
    }

    if use_byte_stride && encoding.simd_byte_class().is_some() {
        shared
            .index
            .write()
            .unwrap()
            .enable_byte_stride_checkpoints(tuning.byte_stride);
    }

    run_sequential_indexer(&file, file_size, delimiter, encoding, config, shared)?;
    Ok(file)
}

fn run_sequential_indexer(
    file: &File,
    file_size: u64,
    delimiter: u8,
    encoding: Encoding,
    config: IndexConfig,
    shared: &SharedState,
) -> Result<(), QuarryError> {
    let backend = if encoding.simd_byte_class().is_some() {
        active_simd_backend()
    } else {
        SimdBackend::Scalar
    };
    {
        let mut index = shared.index.write().unwrap();
        index.telemetry.effective_workers = 1;
        index.telemetry.simd_backend = backend;
    }

    let mut scanner = EncodedScanner::<true>::new(delimiter, encoding)?;
    let chunk_len = encoding.chunk_size(config.chunk_bytes);
    let mut chunk = vec![0u8; chunk_len];
    let mut absolute_start = 0u64;

    if encoding.is_utf16() {
        let mut cursor_file = file;
        loop {
            if shared.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            let read = encoding.read_chunk(&mut cursor_file, &mut chunk)?;
            if read == 0 {
                let mut index = shared.index.write().unwrap();
                scanner.finish(absolute_start, |end| index.record_boundary(end))?;
                index.maximum_width = scanner.maximum_width();
                shared
                    .columns_found
                    .store(index.maximum_width, Ordering::Release);
                shared
                    .rows_scanned
                    .store(index.indexed_rows, Ordering::Release);
                break;
            }
            {
                let mut index = shared.index.write().unwrap();
                scanner.scan_chunk(&chunk[..read], absolute_start, |end| {
                    index.record_boundary(end)
                })?;
                index.indexed_bytes = absolute_start + read as u64;
                index.maximum_width = scanner.maximum_width();
                index.telemetry.chunks_processed += 1;
                shared
                    .columns_found
                    .store(index.maximum_width, Ordering::Release);
                shared
                    .rows_scanned
                    .store(index.indexed_rows, Ordering::Release);
            }
            absolute_start += read as u64;
            shared
                .bytes_scanned
                .store(absolute_start, Ordering::Release);
        }
    } else {
        advise_readahead(file, 0, file_size.min(4 * 1024 * 1024));
        loop {
            if shared.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            let read = read_at_exact_or_eof(file, absolute_start, &mut chunk)?;
            if read == 0 {
                let mut index = shared.index.write().unwrap();
                scanner.finish(absolute_start, |end| index.record_boundary(end))?;
                index.maximum_width = scanner.maximum_width();
                shared
                    .columns_found
                    .store(index.maximum_width, Ordering::Release);
                shared
                    .rows_scanned
                    .store(index.indexed_rows, Ordering::Release);
                break;
            }
            let next_offset = absolute_start + read as u64;
            if next_offset < file_size {
                advise_readahead(
                    file,
                    next_offset,
                    (file_size - next_offset).min(chunk_len as u64 * 2),
                );
            }
            {
                let mut index = shared.index.write().unwrap();
                scanner.scan_chunk(&chunk[..read], absolute_start, |end| {
                    index.record_boundary(end)
                })?;
                index.indexed_bytes = next_offset;
                index.maximum_width = scanner.maximum_width();
                index.telemetry.chunks_processed += 1;
                shared
                    .columns_found
                    .store(index.maximum_width, Ordering::Release);
                shared
                    .rows_scanned
                    .store(index.indexed_rows, Ordering::Release);
            }
            absolute_start = next_offset;
            shared
                .bytes_scanned
                .store(absolute_start, Ordering::Release);
        }
    }

    shared
        .bytes_scanned
        .store(absolute_start, Ordering::Release);
    Ok(())
}

#[derive(Debug)]
struct StateOutcome {
    rows: u64,
    checkpoints: Vec<(u64, u64)>,
    max_width: usize,
    first_record_extra_seps: Option<usize>,
    max_width_after_first: usize,
    trailing_separators: usize,
    end_in_quote: bool,
    error: Option<ParseError>,
}

#[derive(Debug)]
struct ChunkResult {
    id: u64,
    end: u64,
    field_start: StateOutcome,
    quoted: Option<StateOutcome>,
    did_dual_scan: bool,
}

struct MergeState {
    in_quote: bool,
    pending_separators: usize,
    next_id: u64,
    pending: BTreeMap<u64, Result<ChunkResult, QuarryError>>,
}

impl MergeState {
    fn new() -> Self {
        Self {
            in_quote: false,
            pending_separators: 0,
            next_id: 0,
            pending: BTreeMap::new(),
        }
    }

    fn apply_ready(&mut self, shared: &SharedState) -> Result<(), QuarryError> {
        while let Some(result) = self.pending.remove(&self.next_id) {
            self.next_id += 1;
            let chunk = result?;
            let was_in_quote = self.in_quote;
            let outcome = if was_in_quote {
                chunk
                    .quoted
                    .expect("non-initial LF-aligned chunk has quoted outcome")
            } else {
                chunk.field_start
            };

            if let Some(error) = outcome.error {
                return Err(QuarryError::Parse(error));
            }

            {
                let mut index = shared.index.write().unwrap();
                let base_row = index.indexed_rows;
                for (local_row, offset) in outcome.checkpoints {
                    index.record_byte_stride_checkpoint(base_row + local_row, offset);
                }
                index.indexed_rows = base_row + outcome.rows;
                index.indexed_bytes = chunk.end;
                if was_in_quote {
                    if outcome.rows == 0 {
                        self.pending_separators = self
                            .pending_separators
                            .saturating_add(outcome.trailing_separators);
                    } else {
                        let first_width = self
                            .pending_separators
                            .saturating_add(outcome.first_record_extra_seps.unwrap_or(0))
                            .saturating_add(1);
                        index.maximum_width = index
                            .maximum_width
                            .max(first_width)
                            .max(outcome.max_width_after_first);
                        self.pending_separators = outcome.trailing_separators;
                    }
                } else {
                    index.maximum_width = index.maximum_width.max(outcome.max_width);
                    self.pending_separators = outcome.trailing_separators;
                }
                self.in_quote = outcome.end_in_quote;
                index.telemetry.chunks_processed += 1;
                if chunk.did_dual_scan {
                    index.telemetry.dual_scan_chunks += 1;
                }
                if was_in_quote {
                    index.telemetry.quoted_entry_chunks += 1;
                }
                shared
                    .columns_found
                    .store(index.maximum_width, Ordering::Release);
                shared
                    .rows_scanned
                    .store(index.indexed_rows, Ordering::Release);
            }
            shared.bytes_scanned.store(chunk.end, Ordering::Release);
        }
        Ok(())
    }
}

fn find_next_lf(
    file: &File,
    start: u64,
    limit_end: u64,
    cancelled: &AtomicBool,
) -> Result<Option<u64>, QuarryError> {
    let mut pos = start;
    let mut probe = [0u8; ALIGNMENT_PROBE_BYTES];
    while pos < limit_end {
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let want = (limit_end - pos).min(probe.len() as u64) as usize;
        let read = read_at_exact_or_eof(file, pos, &mut probe[..want])?;
        if read < want {
            return Err(QuarryError::SourceChanged);
        }
        if let Some(rel) = memchr(b'\n', &probe[..read]) {
            return Ok(Some(pos + rel as u64));
        }
        pos += read as u64;
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn scan_outcome(
    mut scanner: StructuralScanner<true>,
    buf: &[u8],
    start: u64,
    end: u64,
    is_last: bool,
    byte_stride: u64,
    in_quote_entry: bool,
    stop_flag: &AtomicBool,
) -> Option<StateOutcome> {
    let mut rows = 0u64;
    let mut checkpoints = Vec::new();
    let mut next_target = if in_quote_entry {
        (start / byte_stride)
            .saturating_mul(byte_stride)
            .max(byte_stride)
    } else {
        ((start / byte_stride).saturating_add(1)).saturating_mul(byte_stride)
    };

    let mut error = None;
    let mut offset = 0usize;
    while offset < buf.len() {
        if stop_flag.load(Ordering::Acquire) {
            return None;
        }
        let slice_end = (offset + WORKER_CANCEL_SLICE_BYTES).min(buf.len());
        let res = scanner.scan_chunk(
            &buf[offset..slice_end],
            start + offset as u64,
            |record_end| {
                rows += 1;
                if record_end >= next_target {
                    checkpoints.push((rows, record_end));
                    next_target =
                        ((record_end / byte_stride).saturating_add(1)).saturating_mul(byte_stride);
                }
            },
        );
        if let Err(err) = res {
            error = Some(err);
            break;
        }
        offset = slice_end;
    }

    if error.is_none() && is_last {
        if let Err(err) = scanner.finish(end, |record_end| {
            rows += 1;
            if record_end >= next_target {
                checkpoints.push((rows, record_end));
                next_target =
                    ((record_end / byte_stride).saturating_add(1)).saturating_mul(byte_stride);
            }
        }) {
            error = Some(err);
        }
    }

    let first_record_extra_seps = if rows > 0 {
        Some(scanner.first_record_width().saturating_sub(1))
    } else {
        None
    };

    Some(StateOutcome {
        rows,
        checkpoints,
        max_width: scanner.maximum_width(),
        first_record_extra_seps,
        max_width_after_first: scanner.max_width_after_first(),
        trailing_separators: scanner.current_separators(),
        end_in_quote: scanner.is_in_quote(),
        error,
    })
}

#[allow(clippy::too_many_arguments)]
fn scan_lf_chunk_worker(
    id: u64,
    start: u64,
    end: u64,
    is_last: bool,
    file: &File,
    dialect: ScannerDialect,
    byte_class: ByteEncodingClass,
    byte_stride: u64,
    stop_flag: &AtomicBool,
) -> Option<Result<ChunkResult, QuarryError>> {
    if stop_flag.load(Ordering::Acquire) {
        return None;
    }
    let len = match usize::try_from(end - start) {
        Ok(len) => len,
        Err(_) => return Some(Err(QuarryError::InvalidOption("chunk length overflow"))),
    };
    let mut buf = vec![0u8; len];
    match read_at_exact_or_eof(file, start, &mut buf) {
        Ok(read) if read == len => {}
        Ok(_) => return Some(Err(QuarryError::SourceChanged)),
        Err(error) => return Some(Err(QuarryError::Io(error))),
    }
    if stop_flag.load(Ordering::Acquire) {
        return None;
    }

    let fs_scanner = if start == 0 {
        match StructuralScanner::<true>::with_dialect(dialect, 0, byte_class) {
            Ok(scanner) => scanner,
            Err(err) => return Some(Err(QuarryError::Parse(err))),
        }
    } else {
        match StructuralScanner::<true>::with_dialect_without_bom(dialect, start, byte_class) {
            Ok(scanner) => scanner,
            Err(err) => return Some(Err(QuarryError::Parse(err))),
        }
    };

    let field_start = scan_outcome(
        fs_scanner,
        &buf,
        start,
        end,
        is_last,
        byte_stride,
        false,
        stop_flag,
    )?;

    let (quoted, did_dual_scan) = if start == 0 {
        (None, false)
    } else {
        let has_quote = dialect
            .quote
            .is_some_and(|quote_byte| memchr(quote_byte, &buf).is_some());
        if !has_quote {
            // Quote-free chunk starting inside a quoted field cannot close the quote.
            let trivial = StateOutcome {
                rows: 0,
                checkpoints: Vec::new(),
                max_width: 0,
                first_record_extra_seps: None,
                max_width_after_first: 0,
                trailing_separators: 0,
                end_in_quote: true,
                error: is_last.then_some(ParseError {
                    offset: end,
                    kind: ParseErrorKind::UnterminatedQuote,
                }),
            };
            (Some(trivial), false)
        } else {
            let q_scanner = match StructuralScanner::<true>::at_offset_in_quote(
                dialect,
                start,
                byte_class,
                active_simd_backend(),
            ) {
                Ok(scanner) => scanner,
                Err(err) => return Some(Err(QuarryError::Parse(err))),
            };
            let outcome = scan_outcome(
                q_scanner,
                &buf,
                start,
                end,
                is_last,
                byte_stride,
                true,
                stop_flag,
            )?;
            (Some(outcome), true)
        }
    };

    Some(Ok(ChunkResult {
        id,
        end,
        field_start,
        quoted,
        did_dual_scan,
    }))
}

#[allow(clippy::too_many_arguments)]
fn scan_giant_line_sequential(
    file: &File,
    start: u64,
    min_stop: u64,
    file_size: u64,
    dialect: ScannerDialect,
    byte_class: ByteEncodingClass,
    read_chunk_bytes: usize,
    merge: &mut MergeState,
    shared: &SharedState,
) -> Result<u64, QuarryError> {
    let mut scanner = if start == 0 {
        StructuralScanner::<true>::with_dialect(dialect, 0, byte_class)?
    } else if merge.in_quote {
        let mut s = StructuralScanner::<true>::at_offset_in_quote(
            dialect,
            start,
            byte_class,
            active_simd_backend(),
        )?;
        s.set_separators(merge.pending_separators);
        s
    } else {
        StructuralScanner::<true>::with_dialect_without_bom(dialect, start, byte_class)?
    };

    let mut pos = start;
    let mut buf = vec![0u8; read_chunk_bytes.clamp(64 * 1024, 1024 * 1024)];
    let mut span_bytes = 0u64;

    while pos < file_size {
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(pos);
        }
        let want = (file_size - pos).min(buf.len() as u64) as usize;
        let read = read_at_exact_or_eof(file, pos, &mut buf[..want])?;
        if read < want {
            return Err(QuarryError::SourceChanged);
        }

        let mut slice_len = read;
        let mut hit_lf_after_min = false;
        if pos + read as u64 >= min_stop {
            let search_from = min_stop.saturating_sub(pos) as usize;
            if let Some(rel) = memchr(b'\n', &buf[search_from..read]) {
                slice_len = search_from + rel + 1;
                hit_lf_after_min = true;
            }
        }

        let next_pos = pos + slice_len as u64;
        {
            let mut index = shared.index.write().unwrap();
            scanner.scan_chunk(&buf[..slice_len], pos, |record_end| {
                index.record_boundary(record_end);
            })?;
            if next_pos == file_size {
                scanner.finish(file_size, |record_end| {
                    index.record_boundary(record_end);
                })?;
            }
            index.indexed_bytes = next_pos;
            index.maximum_width = index.maximum_width.max(scanner.maximum_width());
            shared
                .columns_found
                .store(index.maximum_width, Ordering::Release);
            shared
                .rows_scanned
                .store(index.indexed_rows, Ordering::Release);
        }
        span_bytes += slice_len as u64;
        pos = next_pos;
        shared.bytes_scanned.store(pos, Ordering::Release);
        if hit_lf_after_min {
            break;
        }
    }

    {
        let mut index = shared.index.write().unwrap();
        index.telemetry.rescanned_bytes += span_bytes;
        index.telemetry.chunks_processed += 1;
    }
    merge.in_quote = scanner.is_in_quote();
    merge.pending_separators = scanner.current_separators();
    Ok(pos)
}

fn drain_parallel_window(
    merge: &mut MergeState,
    dispatched_count: u64,
    rx: &mpsc::Receiver<Result<ChunkResult, QuarryError>>,
    stop_flag: &AtomicBool,
    shared: &SharedState,
) -> Result<(), QuarryError> {
    while merge.next_id < dispatched_count {
        if shared.cancelled.load(Ordering::Acquire) {
            stop_flag.store(true, Ordering::Release);
            return Ok(());
        }
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(result) => {
                let id = match &result {
                    Ok(chunk) => chunk.id,
                    Err(_) => {
                        stop_flag.store(true, Ordering::Release);
                        return Err(result.unwrap_err());
                    }
                };
                merge.pending.insert(id, result);
                if let Err(err) = merge.apply_ready(shared) {
                    stop_flag.store(true, Ordering::Release);
                    return Err(err);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn run_parallel_indexer(
    file: Arc<File>,
    file_size: u64,
    delimiter: u8,
    byte_class: ByteEncodingClass,
    config: IndexConfig,
    tuning: ParallelTuning,
    shared: &SharedState,
) -> Result<(), QuarryError> {
    let dialect = ScannerDialect::rfc4180(delimiter);
    dialect.validate(byte_class)?;

    let chunk_size = if tuning.allow_small_chunks {
        config.chunk_bytes.max(64)
    } else {
        config
            .chunk_bytes
            .clamp(DEFAULT_INDEX_CHUNK_BYTES, PARALLEL_MAX_CHUNK_BYTES)
    };
    let estimated_chunks = file_size.div_ceil(chunk_size as u64).max(1) as usize;
    let effective_workers = tuning.requested_threads.min(estimated_chunks).max(1);

    {
        let mut index = shared.index.write().unwrap();
        index.enable_byte_stride_checkpoints(tuning.byte_stride);
        index.telemetry.effective_workers = effective_workers;
        index.telemetry.simd_backend = active_simd_backend();
    }

    if file_size == 0 {
        return Ok(());
    }

    let pool = engine_pool();
    let executor = JobExecutor::new(pool, JobPriority::Background, effective_workers);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Result<ChunkResult, QuarryError>>();
    let max_window = (effective_workers * 2).clamp(2, 16);

    let mut merge = MergeState::new();
    let mut cursor = 0u64;
    let mut next_chunk_id = 0u64;

    let run_result: Result<(), QuarryError> = (|| {
        while cursor < file_size {
            if shared.cancelled.load(Ordering::Acquire) {
                stop_flag.store(true, Ordering::Release);
                return Ok(());
            }

            // Drain any already-completed chunks without blocking to keep memory flat.
            while let Ok(result) = rx.try_recv() {
                let id = match &result {
                    Ok(chunk) => chunk.id,
                    Err(_) => {
                        stop_flag.store(true, Ordering::Release);
                        return Err(result.unwrap_err());
                    }
                };
                merge.pending.insert(id, result);
                if let Err(err) = merge.apply_ready(shared) {
                    stop_flag.store(true, Ordering::Release);
                    return Err(err);
                }
            }

            // Bound in-flight window to keep peak RSS well under 64 MiB.
            while (next_chunk_id - merge.next_id) as usize >= max_window {
                if shared.cancelled.load(Ordering::Acquire) {
                    stop_flag.store(true, Ordering::Release);
                    return Ok(());
                }
                match rx.recv_timeout(Duration::from_millis(2)) {
                    Ok(result) => {
                        let id = match &result {
                            Ok(chunk) => chunk.id,
                            Err(_) => {
                                stop_flag.store(true, Ordering::Release);
                                return Err(result.unwrap_err());
                            }
                        };
                        merge.pending.insert(id, result);
                        if let Err(err) = merge.apply_ready(shared) {
                            stop_flag.store(true, Ordering::Release);
                            return Err(err);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }

            let nominal_end = cursor.saturating_add(chunk_size as u64).min(file_size);
            let chunk_end = if nominal_end == file_size {
                file_size
            } else {
                let search_limit = nominal_end
                    .saturating_add(tuning.max_alignment_search)
                    .min(file_size);
                match find_next_lf(&file, nominal_end, search_limit, &shared.cancelled)? {
                    Some(lf_pos) => lf_pos + 1,
                    None if shared.cancelled.load(Ordering::Acquire) => {
                        stop_flag.store(true, Ordering::Release);
                        return Ok(());
                    }
                    None => {
                        // No LF within bounded alignment window (giant single-line record):
                        // drain prior parallel chunks so exact scanner state is confirmed,
                        // stream sequentially across the giant line, then resume parallel scanning.
                        drain_parallel_window(&mut merge, next_chunk_id, &rx, &stop_flag, shared)?;
                        if shared.cancelled.load(Ordering::Acquire) {
                            stop_flag.store(true, Ordering::Release);
                            return Ok(());
                        }
                        cursor = scan_giant_line_sequential(
                            &file,
                            cursor,
                            search_limit,
                            file_size,
                            dialect,
                            byte_class,
                            chunk_size,
                            &mut merge,
                            shared,
                        )?;
                        continue;
                    }
                }
            };

            let start = cursor;
            let end = chunk_end;
            let is_last = end == file_size;
            let id = next_chunk_id;
            next_chunk_id += 1;
            cursor = end;

            let chunk_len = usize::try_from(end - start)
                .map_err(|_| QuarryError::InvalidOption("chunk length overflow"))?;
            let Some(budget_guard) = pool.byte_budget().acquire(chunk_len, &shared.cancelled)
            else {
                stop_flag.store(true, Ordering::Release);
                return Ok(());
            };

            advise_readahead(&file, start, end - start);

            let worker_file = Arc::clone(&file);
            let worker_stop = Arc::clone(&stop_flag);
            let worker_tx = tx.clone();
            let byte_stride = tuning.byte_stride;
            let spawned = executor.spawn(&shared.cancelled, move || {
                let _budget = budget_guard;
                if let Some(result) = scan_lf_chunk_worker(
                    id,
                    start,
                    end,
                    is_last,
                    &worker_file,
                    dialect,
                    byte_class,
                    byte_stride,
                    &worker_stop,
                ) {
                    // Drop the byte-budget guard as soon as raw bytes are freed, before sending.
                    drop(_budget);
                    let _ = worker_tx.send(result);
                }
            });
            if !spawned {
                stop_flag.store(true, Ordering::Release);
                return Ok(());
            }
        }

        drain_parallel_window(&mut merge, next_chunk_id, &rx, &stop_flag, shared)
    })();

    if run_result.is_err() || shared.cancelled.load(Ordering::Acquire) {
        stop_flag.store(true, Ordering::Release);
    }
    drop(tx);
    let drain_result = executor.drain();
    run_result?;
    drain_result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, RwLock};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::{
        IndexConfig, IndexJob, ParallelTuning, SharedState, StructuralIndex, WorkerCompletion,
    };

    #[test]
    fn compacts_checkpoints_to_stay_under_budget() {
        let mut index = StructuralIndex::new(IndexConfig {
            chunk_bytes: 1,
            checkpoint_every: 1,
            memory_budget_bytes: 4 * std::mem::size_of::<super::Checkpoint>(),
        })
        .unwrap();
        for offset in 1..=100 {
            index.record_boundary(offset);
        }
        assert!(index.checkpoints.len() <= index.max_checkpoints);
        assert!(index.memory_bytes() <= 4 * std::mem::size_of::<super::Checkpoint>());
        assert!(index.checkpoint_every > 1);
        assert_eq!(
            index.nearest_checkpoint(100).row % index.checkpoint_every,
            0
        );
    }

    #[test]
    fn byte_stride_checkpoints_compact_under_budget_even_on_dense_newlines() {
        let budget = 8 * std::mem::size_of::<super::Checkpoint>();
        let mut index = StructuralIndex::new(IndexConfig {
            chunk_bytes: 1024,
            checkpoint_every: 4096,
            memory_budget_bytes: budget,
        })
        .unwrap();
        index.enable_byte_stride_checkpoints(16);
        for offset in 1..=10_000_u64 {
            index.record_boundary(offset);
        }
        assert!(index.checkpoints().len() <= index.max_checkpoints);
        assert!(index.memory_bytes() <= budget);
        assert!(index.checkpoint_byte_stride().unwrap() > 16);
        for window in index.checkpoints().windows(2) {
            assert!(window[0].row < window[1].row);
            assert!(window[0].offset < window[1].offset);
        }
        assert_eq!(index.nearest_checkpoint(0).row, 0);
        assert!(index.nearest_checkpoint(10_000).row <= 10_000);
    }

    #[test]
    fn worker_completion_marks_panicked_workers_done() {
        let shared = SharedState {
            index: RwLock::new(StructuralIndex::new(IndexConfig::default()).unwrap()),
            bytes_scanned: AtomicU64::new(0),
            rows_scanned: AtomicU64::new(0),
            columns_found: AtomicUsize::new(0),
            finished_nanos: AtomicU64::new(0),
            done: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            error: Mutex::new(None),
            started: Instant::now(),
            file_size: 0,
        };

        let result = catch_unwind(AssertUnwindSafe(|| {
            let _completion = WorkerCompletion(&shared);
            panic!("simulated worker panic");
        }));

        assert!(result.is_err());
        assert!(shared.done.load(Ordering::Acquire));
        assert!(shared.finished_nanos.load(Ordering::Acquire) > 0);
    }

    #[test]
    fn dropping_an_active_job_cancels_and_joins_its_worker() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-drop-{name}.csv"));
        let contents = b"a,b\n".repeat(250_000);
        fs::write(&path, contents).unwrap();
        let session = crate::Session::open(
            &path,
            crate::OpenOptions {
                rows: 5,
                delimiter: Some(b','),
                sample_bytes: 256,
                ..crate::OpenOptions::default()
            },
        )
        .unwrap();
        let job = IndexJob::start(
            &session,
            IndexConfig {
                chunk_bytes: 1,
                checkpoint_every: 8,
                memory_budget_bytes: 64 * 1024,
            },
            Some(1),
        )
        .unwrap();
        let shared = Arc::clone(&job.shared);
        let deadline = Instant::now() + Duration::from_secs(2);
        let progress = loop {
            let progress = job.progress();
            if progress.rows_scanned >= 100 || progress.done {
                break progress;
            }
            assert!(Instant::now() < deadline, "index did not make progress");
            thread::yield_now();
        };
        assert!(!progress.done, "test file indexed before drop");
        assert_eq!(progress.columns_found, 2);
        assert_eq!(job.snapshot().maximum_width(), 2);
        assert!(job.snapshot().indexed_rows() > 0);
        assert_eq!(session.completed_record_count(), None);

        drop(job);

        assert!(shared.cancelled.load(Ordering::Acquire));
        assert!(shared.done.load(Ordering::Acquire));
        assert!(shared.finished_nanos.load(Ordering::Acquire) > 0);
        assert_eq!(session.completed_record_count(), None);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn changed_source_cannot_return_an_index_or_publish_a_record_count() {
        for change_during_indexing in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("source.csv");
            fs::write(&path, b"a,b\n".repeat(50_000)).unwrap();
            let session = crate::Session::open(
                &path,
                crate::OpenOptions {
                    rows: 5,
                    delimiter: Some(b','),
                    sample_bytes: 256,
                    ..crate::OpenOptions::default()
                },
            )
            .unwrap();
            let change_source = || {
                fs::rename(&path, directory.path().join("original.csv")).unwrap();
                fs::write(&path, b"different,source\n").unwrap();
            };
            if !change_during_indexing {
                change_source();
            }
            let job = session.start_indexing(IndexConfig {
                chunk_bytes: 1,
                ..IndexConfig::default()
            });
            if change_during_indexing {
                let job = job.unwrap();
                change_source();
                assert!(matches!(job.wait(), Err(crate::QuarryError::SourceChanged)));
            } else {
                assert!(matches!(job, Err(crate::QuarryError::SourceChanged)));
            }
            assert_eq!(session.completed_record_count(), None);
        }
    }

    #[test]
    fn parallel_indexer_matches_sequential_across_threads_multiline_quotes_and_encodings() {
        let directory = tempfile::tempdir().unwrap();

        // Build a rich dataset with multiline quoted fields spanning chunk boundaries,
        // doubled quotes, CRLF, varying column counts, and trailing unterminated-LF EOF.
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\xEF\xBB\xBFid,title,notes,extra\n");
        for i in 0..350_u32 {
            if i % 7 == 0 {
                // Multiline quoted field crossing multiple 256-byte chunks with extra columns after close.
                let line = format!(
                    "{i},\"multi\nline\n\"\"quoted\"\"\nvalue {i}\",\"tail\nline\",c4,c5,c6\r\n"
                );
                payload.extend_from_slice(line.as_bytes());
            } else if i % 5 == 0 {
                let line = format!("{i},plain_{i},simple\n");
                payload.extend_from_slice(line.as_bytes());
            } else {
                let line = format!("{i},\"quoted,comma,{i}\",val_{i},z\n");
                payload.extend_from_slice(line.as_bytes());
            }
        }
        payload.extend_from_slice(b"999,\"final\nmultiline\",last_without_lf");

        let path = directory.path().join("multiline.csv");
        fs::write(&path, &payload).unwrap();

        for encoding in [
            crate::Encoding::Utf8,
            crate::Encoding::Latin1,
            crate::Encoding::Windows1252,
        ] {
            let file_bytes = if encoding == crate::Encoding::Utf8 {
                payload.clone()
            } else {
                payload[3..].to_vec()
            };
            fs::write(&path, &file_bytes).unwrap();

            let session = crate::Session::open_with_encoding(
                &path,
                crate::OpenOptions::default(),
                Some(encoding),
            )
            .unwrap();

            let sequential = session
                .start_indexing_with_threads(IndexConfig::default(), Some(1))
                .unwrap()
                .wait()
                .unwrap();

            for threads in [2usize, 4, 8] {
                let parallel = IndexJob::start_with_tuning(
                    &session,
                    IndexConfig {
                        chunk_bytes: 256,
                        ..IndexConfig::default()
                    },
                    ParallelTuning {
                        requested_threads: threads,
                        min_parallel_bytes: 64,
                        byte_stride: 192,
                        max_alignment_search: 256,
                        allow_small_chunks: true,
                    },
                )
                .unwrap()
                .wait()
                .unwrap();

                assert_eq!(
                    parallel.indexed_rows(),
                    sequential.indexed_rows(),
                    "row count mismatch for encoding={encoding} threads={threads}"
                );
                assert_eq!(
                    parallel.maximum_width(),
                    sequential.maximum_width(),
                    "max width mismatch for encoding={encoding} threads={threads}"
                );
                assert_eq!(parallel.indexed_bytes(), sequential.indexed_bytes());
                assert!(parallel.telemetry().byte_stride_checkpoints);
                assert!(parallel.telemetry().dual_scan_chunks > 0);
                assert!(parallel.telemetry().quoted_entry_chunks > 0);
                assert_eq!(parallel.telemetry().rescanned_bytes, 0);

                // Verify random row seeks through byte-stride checkpoints match sequential rows.
                for probe in [
                    0,
                    1,
                    7,
                    42,
                    175,
                    sequential.indexed_rows() - 2,
                    sequential.indexed_rows() - 1,
                ] {
                    assert_eq!(
                        session.read_rows(&parallel, probe, 2).unwrap(),
                        session.read_rows(&sequential, probe, 2).unwrap(),
                        "row {probe} mismatch for encoding={encoding} threads={threads}"
                    );
                }
            }
        }
    }

    #[test]
    fn parallel_indexer_handles_giant_single_line_without_lf() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("giant.csv");

        let mut data = Vec::new();
        data.extend_from_slice(b"a,b,c\n");
        for i in 0..40 {
            data.extend_from_slice(format!("{i},row_{i},ok\n").as_bytes());
        }
        // Giant single-line record (> 128 KiB without LF) containing a wide column count.
        data.extend_from_slice(b"giant,\"");
        data.extend(std::iter::repeat_n(b'x', 140_000));
        data.extend_from_slice(b"\",c3,c4,c5,c6,c7\n");
        for i in 0..40 {
            data.extend_from_slice(format!("after_{i},val_{i},done\n").as_bytes());
        }
        fs::write(&path, &data).unwrap();

        let session = crate::Session::open(
            &path,
            crate::OpenOptions {
                rows: 5,
                delimiter: Some(b','),
                sample_bytes: 256,
                ..crate::OpenOptions::default()
            },
        )
        .unwrap();
        let sequential = session
            .start_indexing_with_threads(IndexConfig::default(), Some(1))
            .unwrap()
            .wait()
            .unwrap();

        let parallel = IndexJob::start_with_tuning(
            &session,
            IndexConfig {
                chunk_bytes: 512,
                ..IndexConfig::default()
            },
            ParallelTuning {
                requested_threads: 4,
                min_parallel_bytes: 64,
                byte_stride: 1024,
                max_alignment_search: 512,
                allow_small_chunks: true,
            },
        )
        .unwrap()
        .wait()
        .unwrap();

        assert_eq!(parallel.indexed_rows(), sequential.indexed_rows());
        assert_eq!(parallel.maximum_width(), sequential.maximum_width());
        assert_eq!(parallel.maximum_width(), 7);
        assert_eq!(parallel.indexed_bytes(), sequential.indexed_bytes());
        assert!(parallel.telemetry().rescanned_bytes >= 140_000);
        for row in [0, 41, 42, sequential.indexed_rows() - 1] {
            assert_eq!(
                session.read_rows(&parallel, row, 1).unwrap(),
                session.read_rows(&sequential, row, 1).unwrap()
            );
        }
    }

    #[test]
    fn parallel_indexer_reports_exact_earliest_parse_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("malformed.csv");

        // Place two errors in different chunks past Session::open's 1 MiB bootstrap read:
        // parallel must always report the earliest one with exact kind and byte offset.
        let mut data = Vec::with_capacity(1_400_000);
        data.extend_from_slice(b"a,b\n");
        for i in 0..70_000 {
            data.extend_from_slice(format!("{i},ok_row_{i:05}\n").as_bytes());
        }
        data.extend_from_slice(b"1,\"bad\"trailing\n");
        for i in 0..5_000 {
            data.extend_from_slice(format!("{i},mid_row_{i:05}\n").as_bytes());
        }
        data.extend_from_slice(b"2,bad\"quote\n");
        fs::write(&path, &data).unwrap();

        let session = crate::Session::open(
            &path,
            crate::OpenOptions {
                rows: 5,
                delimiter: Some(b','),
                sample_bytes: 256,
                ..crate::OpenOptions::default()
            },
        )
        .unwrap();
        let seq_err = session
            .start_indexing_with_threads(IndexConfig::default(), Some(1))
            .unwrap()
            .wait()
            .unwrap_err();
        let crate::QuarryError::Parse(expected_parse) = seq_err else {
            panic!("expected parse error, got {seq_err:?}");
        };

        for threads in [2usize, 4, 8] {
            let par_err = IndexJob::start_with_tuning(
                &session,
                IndexConfig {
                    chunk_bytes: 32 * 1024,
                    ..IndexConfig::default()
                },
                ParallelTuning {
                    requested_threads: threads,
                    min_parallel_bytes: 64,
                    byte_stride: 16 * 1024,
                    max_alignment_search: 4 * 1024,
                    allow_small_chunks: true,
                },
            )
            .unwrap()
            .wait()
            .unwrap_err();
            let crate::QuarryError::Parse(actual_parse) = par_err else {
                panic!("expected parse error for threads={threads}, got {par_err:?}");
            };
            assert_eq!(actual_parse, expected_parse);
        }
    }

    #[test]
    fn parallel_indexer_rejects_zero_threads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tiny.csv");
        fs::write(&path, b"a,b\n1,2\n").unwrap();
        let session = crate::Session::open(
            &path,
            crate::OpenOptions {
                rows: 5,
                delimiter: Some(b','),
                sample_bytes: 256,
                ..crate::OpenOptions::default()
            },
        )
        .unwrap();
        assert!(matches!(
            session.start_indexing_with_threads(IndexConfig::default(), Some(0)),
            Err(crate::QuarryError::InvalidOption(_))
        ));
    }

    #[test]
    fn scale_arithmetic_handles_1_1_trillion_rows_and_over_16_tb_offsets() {
        const TARGET_ROWS: u64 = 1_099_511_627_776; // 2^40 (~1.0995 trillion lines)
        const TARGET_BYTES: u64 = 20 * 1024_u64 * 1024 * 1024 * 1024; // 20 TiB (> 16 TiB)
        const AVG_BYTES_PER_ROW: u64 = TARGET_BYTES / TARGET_ROWS; // 20 bytes/row

        let mut index = StructuralIndex::new(IndexConfig::default()).unwrap();
        assert_eq!(index.max_checkpoints, 1_048_576); // 16 MiB / 16 bytes

        // Compute the compacted checkpoint spacing that keeps <= max_checkpoints across TARGET_ROWS.
        let mut interval = index.checkpoint_every;
        while TARGET_ROWS / interval >= index.max_checkpoints as u64 {
            interval = interval.checked_mul(2).expect("interval must not overflow");
        }
        assert_eq!(interval, 2_097_152); // 2^21 rows per checkpoint

        // Populate a representative compacted checkpoint table obeying StructuralIndex invariants.
        let count = (TARGET_ROWS / interval) as usize + 1;
        assert!(count <= index.max_checkpoints);
        index.checkpoint_every = interval;
        index.checkpoints = (0..count as u64)
            .map(|slot| {
                let row = slot.checked_mul(interval).unwrap();
                let offset = row.checked_mul(AVG_BYTES_PER_ROW).unwrap();
                super::Checkpoint { row, offset }
            })
            .collect();
        index.checkpoints.shrink_to_fit();
        index.indexed_rows = TARGET_ROWS;
        index.indexed_bytes = TARGET_BYTES;

        assert!(index.memory_bytes() <= IndexConfig::default().memory_budget_bytes);

        // Deterministic sampling across small, 32-bit boundary, 100M, 1.1T, and u64::MAX queries.
        let mut probes = vec![
            0,
            1,
            4095,
            4096,
            u32::MAX as u64 - 1,
            u32::MAX as u64,
            u32::MAX as u64 + 1,
            100_000_000,
            TARGET_ROWS - 1,
            TARGET_ROWS,
            TARGET_ROWS + 123_456,
            u64::MAX,
        ];
        let mut state = 0x167B_1100_0000_0001_u64;
        for _ in 0..2_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            probes.push(state % (TARGET_ROWS + 1));
        }

        for row in probes {
            let cp = index.nearest_checkpoint(row);
            assert!(cp.row <= row, "checkpoint row {} > query row {row}", cp.row);
            assert_eq!(cp.row % index.checkpoint_every(), 0);
            assert_eq!(cp.offset, cp.row.checked_mul(AVG_BYTES_PER_ROW).unwrap());
            if row <= TARGET_ROWS {
                assert!(
                    row - cp.row < index.checkpoint_every(),
                    "gap {} >= interval {}",
                    row - cp.row,
                    index.checkpoint_every()
                );
            }
        }
    }

    #[test]
    fn compaction_interval_doubling_saturates_safely_near_u64_max() {
        let mut index = StructuralIndex::new(IndexConfig {
            chunk_bytes: 1,
            checkpoint_every: 1_u64 << 62,
            memory_budget_bytes: 2 * std::mem::size_of::<super::Checkpoint>(),
        })
        .unwrap();
        index.checkpoints.push(super::Checkpoint {
            row: 1_u64 << 62,
            offset: 1_u64 << 62,
        });
        index.indexed_rows = (1_u64 << 63) - 1;
        // Trigger compaction at 2^63: doubling 2^62 yields 2^63 cleanly without overflow.
        index.record_boundary(u64::MAX - 1);
        assert_eq!(index.checkpoint_every(), 1_u64 << 63);
        assert!(index.checkpoints().len() <= index.max_checkpoints);
        assert_eq!(index.nearest_checkpoint(u64::MAX).row, 1_u64 << 63);
    }

    /// Unix filesystems (APFS, ext4, XFS, btrfs) create zero-allocation sparse holes on
    /// seek + write; on Windows a non-sparse file would physically allocate the entire gap
    /// without `FSCTL_SET_SPARSE`, so this test is restricted to Unix targets.
    #[cfg(unix)]
    #[test]
    fn sparse_file_seek_and_read_near_large_offset_without_scanning_hole() {
        use std::io::{Read, Seek, SeekFrom, Write};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sparse-scale.csv");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        let head = b"id,value\n0,head\n";
        file.write_all(head).unwrap();

        // Try a >16 TB-class offset (16 TiB - 64 KiB fits APFS, ext4, XFS, btrfs), falling
        // back to >4 GiB (u32::MAX + 1 MiB) if the host test filesystem caps smaller.
        let candidates = [
            16_u64 * 1024 * 1024 * 1024 * 1024 - 64 * 1024,
            u64::from(u32::MAX) + 1024 * 1024,
        ];
        let tail = b"1099511627775,\"tail,quoted\"\n";
        let mut tail_offset = None;
        for offset in candidates {
            if file.seek(SeekFrom::Start(offset)).is_ok() && file.write_all(tail).is_ok() {
                tail_offset = Some(offset);
                break;
            }
        }
        let tail_offset = tail_offset.expect("filesystem must support >4 GiB sparse seek");
        assert!(tail_offset > u64::from(u32::MAX));

        // Seek directly to the tail offset and scan/parse without touching the sparse hole.
        file.seek(SeekFrom::Start(tail_offset)).unwrap();
        let mut buf = vec![0u8; tail.len()];
        file.read_exact(&mut buf).unwrap();

        let mut scanner = quarry_delimited::RecordScanner::at_offset(b',', tail_offset).unwrap();
        let mut ends = Vec::new();
        scanner
            .scan_chunk(&buf, tail_offset, |end| ends.push(end))
            .unwrap();
        assert!(
            !scanner
                .finish(tail_offset + buf.len() as u64, |end| ends.push(end))
                .unwrap()
        );
        assert_eq!(ends, vec![tail_offset + tail.len() as u64]);

        let parsed = quarry_delimited::parse_record(&buf, b',').unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].as_ref(), b"1099511627775");
        assert_eq!(parsed[1].as_ref(), b"tail,quoted");
    }
}
