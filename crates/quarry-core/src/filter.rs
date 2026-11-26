use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use memchr::memchr;
use quarry_delimited::{
    ByteEncodingClass, ParseError, ParseErrorKind, ScannerDialect, StructuralScanner,
    active_simd_backend,
};

use crate::Encoding;
use crate::case::ByteMatcher;
use crate::encoding::RecordScanner;
use crate::export::source_matches_stamp;
use crate::pool::{JobExecutor, JobPriority, engine_pool, resolve_worker_count};
use crate::positional_io::{advise_readahead, read_at_exact_or_eof};
use crate::sort::numeric_key;
use crate::source_stamp::SourceStamp;
use crate::{
    CaseSensitivity, DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, QuarryError, Session,
    parse_encoded_record_with_bom,
};

/// Immutable sparse working values, keyed by physical source row and column.
/// Cloning a snapshot shares the cell bytes instead of copying them.
pub type FilterEdits = Arc<BTreeMap<(u64, usize), Arc<Vec<u8>>>>;

#[derive(Debug, Clone)]
struct FilterSource {
    path: PathBuf,
    stamp: SourceStamp,
    delimiter: u8,
    data_start: u64,
    encoding: Encoding,
    section_range: (u64, u64),
    strip_bom_at_zero: bool,
}

impl FilterSource {
    fn check(&self) -> Result<(), QuarryError> {
        let file = File::open(&self.path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
        self.check_file(&file)
    }

    fn check_file(&self, file: &File) -> Result<(), QuarryError> {
        if source_matches_stamp(file, &self.path, &self.stamp)? {
            Ok(())
        } else {
            Err(QuarryError::SourceChanged)
        }
    }
}

const DEFAULT_FILTER_INDEX_MEMORY_BUDGET_BYTES: usize = 16 * 1024 * 1024;
const MATCH_PUBLISH_BATCH: usize = 256;
const PARALLEL_FILTER_MIN_BYTES: u64 = 4 * 1024 * 1024;
const PARALLEL_FILTER_CHUNK_BYTES: usize = 2 * 1024 * 1024;
const FILTER_ALIGNMENT_PROBE_BYTES: usize = 64 * 1024;
const FILTER_MAX_ALIGNMENT_SEARCH: u64 = 256 * 1024;
const FILTER_WORKER_CANCEL_SLICE: usize = 256 * 1024;
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Clone, Copy)]
struct FilterParallelTuning {
    requested_threads: usize,
    min_parallel_bytes: u64,
    chunk_bytes: usize,
    max_alignment_search: u64,
    #[cfg(test)]
    delay_chunk_zero: Option<Duration>,
}

impl FilterParallelTuning {
    fn for_production(requested_threads: usize) -> Self {
        Self {
            requested_threads,
            min_parallel_bytes: PARALLEL_FILTER_MIN_BYTES,
            chunk_bytes: PARALLEL_FILTER_CHUNK_BYTES,
            max_alignment_search: FILTER_MAX_ALIGNMENT_SEARCH,
            #[cfg(test)]
            delay_chunk_zero: None,
        }
    }
}

#[derive(Clone, Copy)]
struct FilterScanConfig {
    chunk_bytes: usize,
    memory_budget_bytes: usize,
    max_record_bytes: usize,
}

const DEFAULT_FILTER_SCAN_CONFIG: FilterScanConfig = FilterScanConfig {
    chunk_bytes: DEFAULT_READ_CHUNK,
    memory_budget_bytes: DEFAULT_FILTER_INDEX_MEMORY_BUDGET_BYTES,
    max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
};

#[derive(Clone, Copy)]
struct FilterReadConfig {
    chunk_bytes: usize,
    max_record_bytes: usize,
}

const DEFAULT_FILTER_READ_CONFIG: FilterReadConfig = FilterReadConfig {
    chunk_bytes: DEFAULT_READ_CHUNK,
    max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOperator {
    Contains,
    Equals,
    NotEquals,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
    Between,
}

impl FilterOperator {
    pub const fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::GreaterThan
                | Self::GreaterThanOrEqual
                | Self::LessThan
                | Self::LessThanOrEqual
                | Self::Between
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterPredicate {
    pub column: usize,
    pub operator: FilterOperator,
    pub value: Vec<u8>,
    /// Inclusive upper bound for Between; must be None for every other operator.
    pub upper_bound: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterQuery {
    pub predicates: Vec<FilterPredicate>,
    pub case_sensitivity: CaseSensitivity,
}

impl FilterQuery {
    /// Check every rule before replacing an active filter or starting a worker.
    pub fn validate(&self) -> Result<(), QuarryError> {
        validate_query(self)
    }

    pub fn single(column: usize, operator: FilterOperator, value: Vec<u8>) -> Self {
        Self::single_with_case(column, operator, value, CaseSensitivity::Sensitive)
    }

    pub fn single_with_case(
        column: usize,
        operator: FilterOperator,
        value: Vec<u8>,
        case_sensitivity: CaseSensitivity,
    ) -> Self {
        Self {
            predicates: vec![FilterPredicate {
                column,
                operator,
                value,
                upper_bound: None,
            }],
            case_sensitivity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterMatch {
    pub match_ordinal: u64,
    pub row: u64,
    pub record_offset: u64,
    pub fields: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterReadOutcome {
    Complete(Vec<FilterMatch>),
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
pub struct FilterReadProgress {
    pub bytes_scanned: u64,
    pub rows_scanned: u64,
    pub matches_read: u64,
    pub total_bytes: u64,
    pub elapsed: Duration,
    pub done: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FilterCheckpoint {
    match_ordinal: u64,
    row: u64,
    offset: u64,
}

#[derive(Debug)]
struct FilterReadPlan {
    query: FilterQuery,
    edits: FilterEdits,
    source: Option<FilterSource>,
    scan_limit: Option<u64>,
    checkpoint: FilterCheckpoint,
    start_match: u64,
    target_end: u64,
    result_capacity: usize,
}

#[derive(Debug, Clone)]
pub struct FilterIndex {
    query: FilterQuery,
    edits: FilterEdits,
    source: Option<FilterSource>,
    scan_limit: Option<u64>,
    complete: bool,
    checkpoints: Vec<FilterCheckpoint>,
    checkpoint_every: u64,
    max_checkpoints: usize,
    matches_found: u64,
}

impl FilterIndex {
    fn new(query: FilterQuery, memory_budget_bytes: usize) -> Result<Self, QuarryError> {
        let checkpoint_capacity = memory_budget_bytes / std::mem::size_of::<FilterCheckpoint>();
        if checkpoint_capacity < 2 {
            return Err(QuarryError::InvalidOption(
                "filter index memory budget must fit at least two checkpoints",
            ));
        }
        let max_checkpoints = 1_usize << checkpoint_capacity.ilog2();
        Ok(Self {
            query,
            edits: Arc::default(),
            source: None,
            scan_limit: None,
            complete: false,
            checkpoints: Vec::with_capacity(max_checkpoints.min(4)),
            checkpoint_every: 1,
            max_checkpoints,
            matches_found: 0,
        })
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub(crate) fn set_complete(&mut self, complete: bool) {
        self.complete = complete;
    }

    /// Returns true only when this filter index completed an unbounded whole-file scan without
    /// edits against the exact source file and framing expected by Live Watch Mode.
    pub(crate) fn is_complete_for_watch(
        &self,
        path: &Path,
        stamp: &SourceStamp,
        delimiter: u8,
        data_start: u64,
        encoding: Encoding,
    ) -> bool {
        self.complete
            && self.scan_limit.is_none()
            && self.edits.is_empty()
            && self.source.as_ref().is_some_and(|source| {
                source.path == path
                    && source.stamp == *stamp
                    && source.delimiter == delimiter
                    && source.data_start == data_start
                    && source.encoding == encoding
                    && source.section_range == (0, stamp.file_size())
                    && source.strip_bom_at_zero
            })
    }

    pub(crate) fn record_match(&mut self, row: u64, offset: u64) {
        let match_ordinal = self.matches_found;
        self.matches_found += 1;
        if !match_ordinal.is_multiple_of(self.checkpoint_every) {
            return;
        }
        if self.checkpoints.len() == self.max_checkpoints {
            self.checkpoint_every = self.checkpoint_every.saturating_mul(2);
            let interval = self.checkpoint_every;
            self.checkpoints
                .retain(|checkpoint| checkpoint.match_ordinal.is_multiple_of(interval));
        }
        if match_ordinal.is_multiple_of(self.checkpoint_every) {
            self.checkpoints.push(FilterCheckpoint {
                match_ordinal,
                row,
                offset,
            });
        }
    }

    /// Rolls back a single trailing match when Live Watch rolls back an unterminated baseline row.
    pub(crate) fn rollback_last_match_for_row(&mut self, row: u64) {
        if self.matches_found == 0 {
            return;
        }
        self.matches_found -= 1;
        while self
            .checkpoints
            .last()
            .is_some_and(|cp| cp.row >= row || cp.match_ordinal >= self.matches_found)
        {
            self.checkpoints.pop();
        }
    }

    /// Updates `FilterIndex` source stamp and scan bounds when Live Watch stops.
    pub(crate) fn update_source_after_watch(
        &mut self,
        path: PathBuf,
        stamp: SourceStamp,
        delimiter: u8,
        data_start: u64,
        encoding: Encoding,
        complete: bool,
    ) {
        let file_size = stamp.file_size();
        self.source = Some(FilterSource {
            path,
            stamp,
            delimiter,
            data_start,
            encoding,
            section_range: (0, file_size),
            strip_bom_at_zero: true,
        });
        self.scan_limit = None;
        self.complete = complete;
    }

    /// Creates an empty live-watch `FilterIndex` bound to the watched file's framing.
    pub(crate) fn new_for_watch(
        query: FilterQuery,
        path: PathBuf,
        stamp: SourceStamp,
        delimiter: u8,
        data_start: u64,
        encoding: Encoding,
    ) -> Result<Self, QuarryError> {
        validate_query(&query)?;
        let mut index = Self::new(query, DEFAULT_FILTER_INDEX_MEMORY_BUDGET_BYTES)?;
        let file_size = stamp.file_size();
        index.source = Some(FilterSource {
            path,
            stamp,
            delimiter,
            data_start,
            encoding,
            section_range: (0, file_size),
            strip_bom_at_zero: true,
        });
        Ok(index)
    }

    /// Reads filtered matches directly from a pinned open file descriptor bounded by `committed_end`
    /// using positional `pread`/`seek_read` (safe across concurrent appends and rename rotation).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn read_matches_from_file(
        &self,
        file: &File,
        delimiter: u8,
        encoding: Encoding,
        committed_end: u64,
        start_match: u64,
        count: usize,
        max_record_bytes: usize,
    ) -> Result<Vec<FilterMatch>, QuarryError> {
        let Some(mut plan) = prepare_filtered_read(self, start_match, count)? else {
            return Ok(Vec::new());
        };
        plan.scan_limit = Some(committed_end);
        let matchers = compile_matchers(&plan.query)?;
        let groups = predicate_groups(&plan.query);
        let mut scanner = RecordScanner::at_offset_with_bom_handling(
            delimiter,
            plan.checkpoint.offset,
            encoding,
            true,
        )?;
        let mut chunk = vec![0; encoding.chunk_size(DEFAULT_READ_CHUNK)];
        let mut absolute_start = plan.checkpoint.offset;
        let mut row_number = plan.checkpoint.row;
        let mut record_start = plan.checkpoint.offset;
        let mut match_ordinal = plan.checkpoint.match_ordinal;
        let mut record = Vec::new();
        let mut rows = Vec::with_capacity(plan.result_capacity);

        while match_ordinal < plan.target_end {
            let remaining = committed_end.saturating_sub(absolute_start);
            if remaining == 0 {
                break;
            }
            if encoding.is_utf16() && remaining == 1 {
                return Err(QuarryError::SourceChanged);
            }
            let mut cap = usize::try_from(remaining)
                .unwrap_or(chunk.len())
                .min(chunk.len());
            if encoding.is_utf16() {
                cap &= !1;
            }
            if cap == 0 {
                return Err(QuarryError::SourceChanged);
            }
            let read = read_at_exact_or_eof(file, absolute_start, &mut chunk[..cap])?;
            if read < cap {
                // Any short read below the committed frontier indicates concurrent truncation.
                return Err(QuarryError::SourceChanged);
            }
            let effective_read = read;
            let mut segment_start = 0;
            let mut deferred_error = None;
            scanner.scan_chunk(&chunk[..effective_read], absolute_start, |absolute_end| {
                let local_end = (absolute_end - absolute_start) as usize;
                if deferred_error.is_none() && match_ordinal < plan.target_end {
                    record.extend_from_slice(&chunk[segment_start..local_end]);
                    if record.len() > max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    } else {
                        match matching_fields_with_edits(
                            &record,
                            delimiter,
                            row_number,
                            record_start == 0,
                            &plan.query,
                            &matchers,
                            &groups,
                            &plan.edits,
                            encoding,
                        ) {
                            Ok(Some(fields)) => {
                                if match_ordinal >= plan.start_match {
                                    rows.push(FilterMatch {
                                        match_ordinal,
                                        row: row_number,
                                        record_offset: record_start,
                                        fields: fields
                                            .into_iter()
                                            .map(|field| field.into_owned())
                                            .collect(),
                                    });
                                }
                                match_ordinal += 1;
                            }
                            Ok(None) => {}
                            Err(error) => deferred_error = Some(error),
                        }
                    }
                }
                record.clear();
                row_number += 1;
                record_start = absolute_end;
                segment_start = local_end;
            })?;
            if let Some(error) = deferred_error {
                return Err(error);
            }
            if match_ordinal >= plan.target_end {
                break;
            }
            record.extend_from_slice(&chunk[segment_start..effective_read]);
            if record.len() > max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: max_record_bytes,
                });
            }
            absolute_start += effective_read as u64;
        }

        Ok(rows)
    }

    fn nearest_checkpoint(&self, match_ordinal: u64) -> Option<FilterCheckpoint> {
        let position = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.match_ordinal <= match_ordinal)
            .checked_sub(1)?;
        self.checkpoints.get(position).copied()
    }

    pub fn query(&self) -> &FilterQuery {
        &self.query
    }

    pub fn matches_found(&self) -> u64 {
        self.matches_found
    }

    pub fn memory_bytes(&self) -> usize {
        self.checkpoints.capacity() * std::mem::size_of::<FilterCheckpoint>()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FilterProgress {
    pub bytes_scanned: u64,
    pub rows_scanned: u64,
    pub matches_found: u64,
    pub file_size: u64,
    pub elapsed: Duration,
    pub done: bool,
    pub cancelled: bool,
}

struct SharedState {
    index: RwLock<FilterIndex>,
    bytes_scanned: AtomicU64,
    rows_scanned: AtomicU64,
    matches_found: AtomicU64,
    finished_nanos: AtomicU64,
    done: AtomicBool,
    cancel_requested: AtomicBool,
    cancelled: AtomicBool,
    error: Mutex<Option<String>>,
    started: Instant,
    section_base: u64,
    scan_end: u64,
    file_size: u64,
    bounded_scan: bool,
    strip_bom_at_zero: bool,
}

impl SharedState {
    fn new(
        index: FilterIndex,
        section_base: u64,
        scan_end: u64,
        bounded_scan: bool,
        strip_bom_at_zero: bool,
    ) -> Self {
        Self {
            index: RwLock::new(index),
            bytes_scanned: AtomicU64::new(0),
            rows_scanned: AtomicU64::new(0),
            matches_found: AtomicU64::new(0),
            finished_nanos: AtomicU64::new(0),
            done: AtomicBool::new(false),
            cancel_requested: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            error: Mutex::new(None),
            started: Instant::now(),
            section_base,
            scan_end,
            file_size: scan_end.saturating_sub(section_base),
            bounded_scan,
            strip_bom_at_zero,
        }
    }
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

pub struct FilterJob {
    shared: Arc<SharedState>,
    handle: Option<JoinHandle<Result<(), QuarryError>>>,
}

impl FilterJob {
    #[cfg(test)]
    fn start(
        path: PathBuf,
        file_size: u64,
        delimiter: u8,
        data_start: u64,
        query: FilterQuery,
        config: FilterScanConfig,
    ) -> Result<Self, QuarryError> {
        Self::start_with_edits_and_tuning(
            path,
            file_size,
            delimiter,
            data_start,
            query,
            config,
            Arc::default(),
            None,
            Encoding::Utf8,
            FilterParallelTuning::for_production(1),
            false,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn start_with_edits(
        path: PathBuf,
        file_size: u64,
        delimiter: u8,
        data_start: u64,
        query: FilterQuery,
        config: FilterScanConfig,
        edits: FilterEdits,
        expected: Option<SourceStamp>,
        encoding: Encoding,
    ) -> Result<Self, QuarryError> {
        Self::start_with_edits_and_tuning(
            path,
            file_size,
            delimiter,
            data_start,
            query,
            config,
            edits,
            expected,
            encoding,
            FilterParallelTuning::for_production(1),
            false,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn start_with_edits_and_tuning(
        path: PathBuf,
        file_size: u64,
        delimiter: u8,
        data_start: u64,
        query: FilterQuery,
        config: FilterScanConfig,
        edits: FilterEdits,
        expected: Option<SourceStamp>,
        encoding: Encoding,
        tuning: FilterParallelTuning,
        bounded_scan: bool,
    ) -> Result<Self, QuarryError> {
        Self::start_with_range_edits_and_tuning(
            path,
            0,
            file_size,
            file_size,
            delimiter,
            data_start,
            query,
            config,
            edits,
            expected,
            encoding,
            tuning,
            bounded_scan,
            false,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_range_edits_and_tuning(
        path: PathBuf,
        section_base: u64,
        section_end: u64,
        scan_end: u64,
        delimiter: u8,
        data_start: u64,
        query: FilterQuery,
        config: FilterScanConfig,
        edits: FilterEdits,
        expected: Option<SourceStamp>,
        encoding: Encoding,
        tuning: FilterParallelTuning,
        bounded_scan: bool,
        is_section: bool,
        strip_bom_at_zero: bool,
    ) -> Result<Self, QuarryError> {
        if config.chunk_bytes == 0 {
            return Err(QuarryError::InvalidOption(
                "filter scan chunk must be non-zero",
            ));
        }
        validate_query(&query)?;
        validate_filter_edits(&edits, data_start, config.max_record_bytes)?;
        let mut index = FilterIndex::new(query.clone(), config.memory_budget_bytes)?;
        let file = File::open(&path)?;
        let source = FilterSource {
            path,
            stamp: expected.unwrap_or(SourceStamp::from_file(&file)?),
            delimiter,
            data_start,
            encoding,
            section_range: (section_base, section_end),
            strip_bom_at_zero,
        };
        source.check_file(&file)?;
        let guard_file = file.try_clone()?;
        index.edits = edits;
        index.source = Some(source.clone());
        // Always retain `scan_limit` for sections (including byte-0 sections) and paused scans so
        // filtered readers never scan past the section's end boundary.
        index.scan_limit = (bounded_scan || is_section).then_some(scan_end);
        let shared = Arc::new(SharedState::new(
            index,
            section_base,
            scan_end,
            bounded_scan,
            strip_bom_at_zero,
        ));
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-filter".into())
            .spawn(move || {
                let _completion = WorkerCompletion(&worker_state);
                let result = if section_base == 0
                    && strip_bom_at_zero
                    && let Some(byte_class) = encoding.simd_byte_class()
                    && tuning.requested_threads > 1
                    && scan_end >= tuning.min_parallel_bytes
                {
                    run_parallel_filter(
                        Arc::new(file),
                        scan_end,
                        delimiter,
                        data_start,
                        &query,
                        config.max_record_bytes,
                        encoding,
                        byte_class,
                        tuning,
                        &worker_state,
                    )
                } else {
                    run_filter(
                        file,
                        delimiter,
                        data_start,
                        &query,
                        config.chunk_bytes,
                        config.max_record_bytes,
                        &worker_state,
                    )
                };
                let result = result.and_then(|()| source.check_file(&guard_file));
                if result.is_ok()
                    && !worker_state.cancelled.load(Ordering::Acquire)
                    && !worker_state.bounded_scan
                    && !is_section
                {
                    worker_state.index.write().unwrap().complete = true;
                }
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

    pub fn progress(&self) -> FilterProgress {
        let done = self.shared.done.load(Ordering::Acquire);
        let finished_nanos = self.shared.finished_nanos.load(Ordering::Acquire);
        FilterProgress {
            bytes_scanned: self.shared.bytes_scanned.load(Ordering::Acquire),
            rows_scanned: self.shared.rows_scanned.load(Ordering::Acquire),
            matches_found: self.shared.matches_found.load(Ordering::Acquire),
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

    pub fn snapshot(&self) -> FilterIndex {
        self.shared.index.read().unwrap().clone()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        if !self.shared.done.load(Ordering::Acquire) {
            self.shared.cancel_requested.store(true, Ordering::Release);
        }
    }

    pub fn wait(mut self) -> Result<FilterIndex, QuarryError> {
        let result = self
            .handle
            .take()
            .expect("filter handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?;
        result?;
        let index = self.shared.index.read().unwrap().clone();
        if let Some(source) = &index.source {
            source.check()?;
        }
        Ok(index)
    }
}

impl Drop for FilterJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

struct FilterReadSharedState {
    bytes_scanned: AtomicU64,
    rows_scanned: AtomicU64,
    matches_read: AtomicU64,
    finished_nanos: AtomicU64,
    done: AtomicBool,
    cancel_requested: AtomicBool,
    cancelled: AtomicBool,
    started: Instant,
    total_bytes: u64,
}

impl FilterReadSharedState {
    fn new(total_bytes: u64) -> Self {
        Self {
            bytes_scanned: AtomicU64::new(0),
            rows_scanned: AtomicU64::new(0),
            matches_read: AtomicU64::new(0),
            finished_nanos: AtomicU64::new(0),
            done: AtomicBool::new(false),
            cancel_requested: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            started: Instant::now(),
            total_bytes,
        }
    }
}

struct FilterReadCompletion<'a>(&'a FilterReadSharedState);

impl Drop for FilterReadCompletion<'_> {
    fn drop(&mut self) {
        let elapsed = self.0.started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.0
            .finished_nanos
            .store(elapsed.max(1), Ordering::Release);
        self.0.done.store(true, Ordering::Release);
    }
}

pub struct FilterReadJob {
    source: Option<FilterSource>,
    shared: Arc<FilterReadSharedState>,
    handle: Option<JoinHandle<Result<FilterReadOutcome, QuarryError>>>,
}

impl FilterReadJob {
    fn start(
        path: PathBuf,
        file_size: u64,
        delimiter: u8,
        plan: Option<FilterReadPlan>,
        config: FilterReadConfig,
    ) -> Result<Self, QuarryError> {
        if config.chunk_bytes == 0 {
            return Err(QuarryError::InvalidOption(
                "filtered read chunk must be non-zero",
            ));
        }
        let file = plan
            .as_ref()
            .map(|plan| open_filtered_file(&path, plan.checkpoint.offset))
            .transpose()?;
        let source = plan.as_ref().and_then(|plan| plan.source.clone());
        if let (Some(source), Some(file)) = (&source, &file) {
            source.check_file(file)?;
        }
        let guard = source.clone();
        let guard_file = file.as_ref().map(File::try_clone).transpose()?;
        let total_bytes = plan.as_ref().map_or(0, |plan| {
            plan.scan_limit
                .unwrap_or(file_size)
                .min(file_size)
                .saturating_sub(plan.checkpoint.offset)
        });
        let shared = Arc::new(FilterReadSharedState::new(total_bytes));
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-filter-read".into())
            .spawn(move || {
                let _completion = FilterReadCompletion(&worker_state);
                let result = match (file, plan) {
                    (Some(file), Some(plan)) => {
                        run_filtered_read(file, delimiter, &plan, config, Some(&worker_state))
                    }
                    (None, None) => Ok(FilterReadOutcome::Complete(Vec::new())),
                    _ => unreachable!("filter read file and plan are paired"),
                };
                let result = result.and_then(|outcome| {
                    if let (Some(source), Some(file)) = (&guard, &guard_file) {
                        source.check_file(file)?;
                    }
                    Ok(outcome)
                });
                if matches!(&result, Ok(FilterReadOutcome::Cancelled)) {
                    worker_state.cancelled.store(true, Ordering::Release);
                }
                result
            })?;
        Ok(Self {
            source,
            shared,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> FilterReadProgress {
        let done = self.shared.done.load(Ordering::Acquire);
        let finished_nanos = self.shared.finished_nanos.load(Ordering::Acquire);
        FilterReadProgress {
            bytes_scanned: self.shared.bytes_scanned.load(Ordering::Acquire),
            rows_scanned: self.shared.rows_scanned.load(Ordering::Acquire),
            matches_read: self.shared.matches_read.load(Ordering::Acquire),
            total_bytes: self.shared.total_bytes,
            elapsed: if finished_nanos == 0 {
                self.shared.started.elapsed()
            } else {
                Duration::from_nanos(finished_nanos)
            },
            done,
            cancelled: self.shared.cancelled.load(Ordering::Acquire),
        }
    }

    pub fn cancel(&self) {
        if !self.shared.done.load(Ordering::Acquire) {
            self.shared.cancel_requested.store(true, Ordering::Release);
        }
    }

    pub fn cancel_without_waiting(mut self) {
        self.cancel();
        drop(self.handle.take());
    }

    pub fn wait(mut self) -> Result<FilterReadOutcome, QuarryError> {
        let outcome = self
            .handle
            .take()
            .expect("filter read handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)??;
        if let Some(source) = &self.source {
            source.check()?;
        }
        Ok(outcome)
    }
}

impl Drop for FilterReadJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

impl Session {
    pub fn start_filter(&self, query: FilterQuery) -> Result<FilterJob, QuarryError> {
        self.start_filter_with_edits(query, Arc::default())
    }

    /// Filters an immutable snapshot of working cell values without saving the source.
    pub fn start_filter_with_edits(
        &self,
        query: FilterQuery,
        edits: FilterEdits,
    ) -> Result<FilterJob, QuarryError> {
        self.start_filter_with_edits_and_threads(query, edits, None)
    }

    /// Filters an immutable snapshot of working cell values with an explicit worker count.
    pub fn start_filter_with_edits_and_threads(
        &self,
        query: FilterQuery,
        edits: FilterEdits,
        threads: Option<usize>,
    ) -> Result<FilterJob, QuarryError> {
        self.start_filter_with_edits_threads_and_limit(query, edits, threads, None)
    }

    /// Filters an immutable snapshot of working cell values up to an optional committed byte limit
    /// (for example while background indexing is paused).
    pub fn start_filter_with_edits_and_limit(
        &self,
        query: FilterQuery,
        edits: FilterEdits,
        max_bytes: Option<u64>,
    ) -> Result<FilterJob, QuarryError> {
        self.start_filter_with_edits_threads_and_limit(query, edits, None, max_bytes)
    }

    fn start_filter_with_edits_threads_and_limit(
        &self,
        query: FilterQuery,
        edits: FilterEdits,
        threads: Option<usize>,
        max_bytes: Option<u64>,
    ) -> Result<FilterJob, QuarryError> {
        let requested_threads = resolve_worker_count(threads)?;
        self.ensure_source_unchanged()?;
        let (section_base, section_end) = self.active_byte_range();
        let strip_bom_at_zero = !self.section_is_interior();
        let data_start = u64::from(self.dialect.has_header && strip_bom_at_zero);
        // `max_bytes` is an absolute file offset (e.g. `StructuralIndex::committed_record_end()`),
        // so clamp it within `[section_base, section_end]` instead of adding `section_base` again.
        let scan_limit = max_bytes
            .map(|limit| limit.clamp(section_base, section_end))
            .unwrap_or(section_end);
        FilterJob::start_with_range_edits_and_tuning(
            self.path.clone(),
            section_base,
            section_end,
            scan_limit,
            self.dialect.delimiter,
            data_start,
            query,
            DEFAULT_FILTER_SCAN_CONFIG,
            edits,
            Some(self.source_stamp.clone()),
            self.encoding,
            FilterParallelTuning::for_production(requested_threads),
            scan_limit < section_end,
            self.is_section(),
            strip_bom_at_zero,
        )
    }

    fn check_filter_index(&self, index: &FilterIndex) -> Result<(), QuarryError> {
        self.ensure_source_unchanged()?;
        let (section_base, section_end) = self.active_byte_range();
        let strip_bom_at_zero = !self.section_is_interior();
        let expected_data_start = u64::from(self.dialect.has_header && strip_bom_at_zero);
        if let Some(source) = &index.source
            && (source.path != self.path
                || source.stamp != self.source_stamp
                || source.delimiter != self.dialect.delimiter
                || source.data_start != expected_data_start
                || source.encoding != self.encoding
                || source.section_range != (section_base, section_end)
                || source.strip_bom_at_zero != strip_bom_at_zero)
        {
            return Err(QuarryError::SourceChanged);
        }
        Ok(())
    }

    pub fn read_filtered_rows(
        &self,
        index: &FilterIndex,
        start_match: u64,
        count: usize,
    ) -> Result<Vec<FilterMatch>, QuarryError> {
        self.check_filter_index(index)?;
        read_filtered_rows(
            &self.path,
            self.dialect.delimiter,
            index,
            start_match,
            count,
            DEFAULT_MAX_RECORD_BYTES,
        )
    }

    pub fn start_filtered_read(
        &self,
        index: &FilterIndex,
        start_match: u64,
        count: usize,
    ) -> Result<FilterReadJob, QuarryError> {
        self.check_filter_index(index)?;
        let plan = prepare_filtered_read(index, start_match, count)?;
        let (_, section_end) = self.active_byte_range();
        FilterReadJob::start(
            self.path.clone(),
            section_end,
            self.dialect.delimiter,
            plan,
            DEFAULT_FILTER_READ_CONFIG,
        )
    }
}

pub(crate) fn validate_query(query: &FilterQuery) -> Result<(), QuarryError> {
    compile_matchers(query).map(|_| ())
}

pub(crate) enum PredicateMatcher<'a> {
    Text(Box<ByteMatcher<'a>>),
    Numeric {
        lower: Vec<u8>,
        upper: Option<Vec<u8>>,
    },
}

pub(crate) fn compile_matchers(
    query: &FilterQuery,
) -> Result<Vec<PredicateMatcher<'_>>, QuarryError> {
    if query.predicates.is_empty() {
        return Err(QuarryError::InvalidOption(
            "filter query must contain at least one predicate",
        ));
    }
    query
        .predicates
        .iter()
        .enumerate()
        .map(|(index, predicate)| {
            let invalid = |reason| QuarryError::InvalidFilter {
                rule: index + 1,
                reason,
            };
            if predicate.operator != FilterOperator::Between && predicate.upper_bound.is_some() {
                return Err(invalid("only Between accepts an upper bound"));
            }
            if !predicate.operator.is_numeric() {
                if predicate.operator == FilterOperator::Contains && predicate.value.is_empty() {
                    return Err(invalid("contains filter value must not be empty"));
                }
                return Ok(PredicateMatcher::Text(Box::new(ByteMatcher::new(
                    &predicate.value,
                    query.case_sensitivity,
                ))));
            }
            let invalid_lower = if predicate.operator == FilterOperator::Between {
                "lower bound must be a nonblank number (for example -12.5 or 1e3; exponent -1000000 to 1000000)"
            } else {
                "filter value must be a nonblank number (for example -12.5 or 1e3; exponent -1000000 to 1000000)"
            };
            let lower = numeric_key(&predicate.value)
                .ok()
                .filter(|key| !key.is_empty())
                .ok_or_else(|| invalid(invalid_lower))?;
            let upper = if predicate.operator == FilterOperator::Between {
                let value = predicate
                    .upper_bound
                    .as_ref()
                    .ok_or_else(|| invalid("Between requires an upper bound"))?;
                let key = numeric_key(value)
                    .ok()
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| invalid("upper bound must be a nonblank number (for example -12.5 or 1e3; exponent -1000000 to 1000000)"))?;
                if lower > key {
                    return Err(invalid("Between lower bound must be less than or equal to its upper bound"));
                }
                Some(key)
            } else {
                None
            };
            Ok(PredicateMatcher::Numeric { lower, upper })
        })
        .collect()
}

impl PredicateMatcher<'_> {
    fn matches(
        &self,
        operator: FilterOperator,
        field: &[u8],
        numeric_value: &mut Option<Option<Vec<u8>>>,
    ) -> bool {
        match self {
            Self::Text(matcher) => match operator {
                FilterOperator::Contains => matcher.find(field).is_some(),
                FilterOperator::Equals | FilterOperator::NotEquals => matcher.equals(field),
                _ => unreachable!("numeric operators compile to numeric matchers"),
            },
            Self::Numeric { lower, upper } => {
                if field.is_empty() {
                    return false;
                }
                // A column group can have several rules, but its field is parsed only once.
                let Some(value) = numeric_value
                    .get_or_insert_with(|| numeric_key(field).ok().filter(|key| !key.is_empty()))
                    .as_ref()
                else {
                    return false;
                };
                match operator {
                    FilterOperator::GreaterThan => value > lower,
                    FilterOperator::GreaterThanOrEqual => value >= lower,
                    FilterOperator::LessThan => value < lower,
                    FilterOperator::LessThanOrEqual => value <= lower,
                    FilterOperator::Between => {
                        value >= lower && upper.as_ref().is_some_and(|upper| value <= upper)
                    }
                    _ => unreachable!("text operators compile to text matchers"),
                }
            }
        }
    }
}

pub(crate) fn predicate_groups(query: &FilterQuery) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (index, predicate) in query.predicates.iter().enumerate() {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| query.predicates[group[0]].column == predicate.column)
        {
            group.push(index);
        } else {
            groups.push(vec![index]);
        }
    }
    groups
}

pub(crate) fn validate_filter_edits(
    edits: &FilterEdits,
    data_start: u64,
    max_bytes: usize,
) -> Result<(), QuarryError> {
    for (&(row, _), value) in edits.iter() {
        if row < data_start {
            return Err(QuarryError::InvalidOption(
                "cell edit points to a header row",
            ));
        }
        if value.len() > max_bytes {
            return Err(QuarryError::RecordTooLarge { limit: max_bytes });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn matching_fields_with_edits<'a>(
    record: &'a [u8],
    delimiter: u8,
    physical_row: u64,
    strip_bom: bool,
    query: &FilterQuery,
    matchers: &[PredicateMatcher<'_>],
    groups: &[Vec<usize>],
    edits: &FilterEdits,
    encoding: Encoding,
) -> Result<Option<Vec<Cow<'a, [u8]>>>, QuarryError> {
    let fields =
        parse_encoded_record_with_bom(record, delimiter, physical_row, strip_bom, encoding)?;
    if edits
        .range((physical_row, 0)..=(physical_row, usize::MAX))
        .next_back()
        .is_some_and(|((_, column), _)| *column >= fields.len())
    {
        return Err(QuarryError::InvalidOption(
            "cell edits cannot add missing fields",
        ));
    }
    debug_assert_eq!(query.predicates.len(), matchers.len());
    let matches = groups.iter().all(|group| {
        let Some(&first) = group.first() else {
            return true;
        };
        let Some(field) = fields.get(query.predicates[first].column) else {
            return false;
        };
        let field: &[u8] = edits
            .get(&(physical_row, query.predicates[first].column))
            .map_or(field.as_ref(), |value| value.as_slice());
        let mut has_inclusion = false;
        let mut inclusion_matches = false;
        let mut numeric_value = None;
        for &index in group {
            let operator = query.predicates[index].operator;
            let matches = matchers[index].matches(operator, field, &mut numeric_value);
            if operator == FilterOperator::NotEquals {
                if matches {
                    return false;
                }
            } else {
                has_inclusion = true;
                inclusion_matches |= matches;
            }
        }
        !has_inclusion || inclusion_matches
    });
    Ok(matches.then_some(fields))
}

#[inline]
fn strip_record_ending_fast(record: &[u8]) -> &[u8] {
    if let Some(body) = record.strip_suffix(b"\r\n") {
        body
    } else if let Some(body) = record.strip_suffix(b"\n") {
        body
    } else {
        record
    }
}

/// Fast zero-allocation filter evaluation for unquoted UTF-8 records without edits on the row;
/// falls back to `matching_fields_with_edits` for quoted, non-UTF-8, or edited records.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_matches_fast(
    record: &[u8],
    delimiter: u8,
    physical_row: u64,
    strip_bom: bool,
    query: &FilterQuery,
    matchers: &[PredicateMatcher<'_>],
    groups: &[Vec<usize>],
    max_column: usize,
    edits: &FilterEdits,
    encoding: Encoding,
    field_ranges: &mut Vec<(usize, usize)>,
) -> Result<bool, QuarryError> {
    let row_has_edit = !edits.is_empty()
        && edits
            .range((physical_row, 0)..=(physical_row, usize::MAX))
            .next()
            .is_some();
    if encoding != Encoding::Utf8 || row_has_edit || memchr(b'"', record).is_some() {
        return Ok(matching_fields_with_edits(
            record,
            delimiter,
            physical_row,
            strip_bom,
            query,
            matchers,
            groups,
            edits,
            encoding,
        )?
        .is_some());
    }

    let body = if strip_bom {
        record.strip_prefix(UTF8_BOM).unwrap_or(record)
    } else {
        record
    };
    let body = strip_record_ending_fast(body);

    field_ranges.clear();
    let mut pos = 0usize;
    let target_fields = max_column.saturating_add(1);
    while field_ranges.len() < target_fields {
        match memchr(delimiter, &body[pos..]) {
            Some(rel) => {
                let end = pos + rel;
                field_ranges.push((pos, end));
                pos = end + 1;
            }
            None => {
                field_ranges.push((pos, body.len()));
                break;
            }
        }
    }

    let matched = groups.iter().all(|group| {
        let Some(&first) = group.first() else {
            return true;
        };
        let col = query.predicates[first].column;
        let Some(&(start, end)) = field_ranges.get(col) else {
            return false;
        };
        let field = &body[start..end];
        let mut has_inclusion = false;
        let mut inclusion_matches = false;
        let mut numeric_value = None;
        for &index in group {
            let operator = query.predicates[index].operator;
            let m = matchers[index].matches(operator, field, &mut numeric_value);
            if operator == FilterOperator::NotEquals {
                if m {
                    return false;
                }
            } else {
                has_inclusion = true;
                inclusion_matches |= m;
            }
        }
        !has_inclusion || inclusion_matches
    });

    Ok(matched)
}

fn publish_matches(shared: &SharedState, matches: &mut Vec<(u64, u64)>) {
    if matches.is_empty() {
        return;
    }
    let matches_found = {
        let mut index = shared.index.write().unwrap();
        for &(row, offset) in matches.iter() {
            index.record_match(row, offset);
        }
        index.matches_found()
    };
    matches.clear();
    shared.matches_found.store(matches_found, Ordering::Release);
}

fn run_filter(
    mut file: File,
    delimiter: u8,
    data_start: u64,
    query: &FilterQuery,
    chunk_bytes: usize,
    max_record_bytes: usize,
    shared: &SharedState,
) -> Result<(), QuarryError> {
    let index = shared.index.read().unwrap();
    let edits = index.edits.clone();
    let encoding = index
        .source
        .as_ref()
        .map_or(Encoding::Utf8, |source| source.encoding);
    let strip_bom_at_zero = index
        .source
        .as_ref()
        .is_none_or(|source| source.strip_bom_at_zero);
    drop(index);
    let matchers = compile_matchers(query)?;
    let groups = predicate_groups(query);
    let max_column = query.predicates.iter().map(|p| p.column).max().unwrap_or(0);
    let mut field_ranges = Vec::with_capacity((max_column + 1).min(64));
    let mut scanner = RecordScanner::at_offset_with_bom_handling(
        delimiter,
        shared.section_base,
        encoding,
        strip_bom_at_zero,
    )?;
    if shared.section_base > 0 {
        file.seek(SeekFrom::Start(shared.section_base))?;
    }
    let mut chunk = vec![0; encoding.chunk_size(chunk_bytes)];
    let mut absolute_start = shared.section_base;
    let mut row_number = 0_u64;
    let mut record_start = shared.section_base;
    let mut records_scanned = 0_u64;
    let mut record = Vec::new();
    let mut pending_matches = Vec::with_capacity(MATCH_PUBLISH_BATCH);

    loop {
        if shared.cancel_requested.load(Ordering::Acquire) {
            publish_matches(shared, &mut pending_matches);
            shared.cancelled.store(true, Ordering::Release);
            return Ok(());
        }
        let remaining = shared.scan_end.saturating_sub(absolute_start);
        let read = if remaining == 0 {
            0
        } else {
            let cap = encoding.chunk_size(
                usize::try_from(remaining)
                    .unwrap_or(usize::MAX)
                    .min(chunk.len()),
            );
            encoding.read_chunk(&mut file, &mut chunk[..cap])?
        };
        if read == 0 {
            let mut deferred_error = None;
            let mut cancelled = false;
            let finish_result = scanner.finish(absolute_start, |absolute_end| {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    cancelled = true;
                } else if row_number >= data_start {
                    if record.len() > max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    } else {
                        match record_matches_fast(
                            &record,
                            delimiter,
                            row_number,
                            shared.strip_bom_at_zero && record_start == 0,
                            query,
                            &matchers,
                            &groups,
                            max_column,
                            &edits,
                            encoding,
                            &mut field_ranges,
                        ) {
                            Ok(true) => pending_matches.push((row_number, record_start)),
                            Ok(false) => {}
                            Err(error) => deferred_error = Some(error),
                        }
                    }
                }
                row_number += 1;
                records_scanned += 1;
                record_start = absolute_end;
                record.clear();
            });
            publish_matches(shared, &mut pending_matches);
            shared.bytes_scanned.store(
                absolute_start.saturating_sub(shared.section_base),
                Ordering::Release,
            );
            shared
                .rows_scanned
                .store(records_scanned, Ordering::Release);
            if cancelled {
                shared.cancelled.store(true, Ordering::Release);
                return Ok(());
            }
            if let Some(error) = deferred_error {
                return Err(error);
            }
            finish_result?;
            if !shared.bounded_scan
                && edits
                    .keys()
                    .next_back()
                    .is_some_and(|(row, _)| *row >= row_number)
            {
                return Err(QuarryError::InvalidOption(
                    "cell edit points beyond the source rows",
                ));
            }
            return Ok(());
        }

        let mut segment_start = 0_usize;
        let mut deferred_error = None;
        let mut cancelled = false;
        let scan_result = scanner.scan_chunk(&chunk[..read], absolute_start, |absolute_end| {
            let local_end = (absolute_end - absolute_start) as usize;
            if deferred_error.is_none() && !cancelled {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    cancelled = true;
                } else if row_number >= data_start {
                    if record.is_empty() {
                        let rec = &chunk[segment_start..local_end];
                        if rec.len() > max_record_bytes {
                            deferred_error = Some(QuarryError::RecordTooLarge {
                                limit: max_record_bytes,
                            });
                        } else {
                            match record_matches_fast(
                                rec,
                                delimiter,
                                row_number,
                                shared.strip_bom_at_zero && record_start == 0,
                                query,
                                &matchers,
                                &groups,
                                max_column,
                                &edits,
                                encoding,
                                &mut field_ranges,
                            ) {
                                Ok(true) => {
                                    pending_matches.push((row_number, record_start));
                                    if pending_matches.len() == MATCH_PUBLISH_BATCH {
                                        publish_matches(shared, &mut pending_matches);
                                    }
                                }
                                Ok(false) => {}
                                Err(error) => deferred_error = Some(error),
                            }
                        }
                    } else {
                        record.extend_from_slice(&chunk[segment_start..local_end]);
                        if record.len() > max_record_bytes {
                            deferred_error = Some(QuarryError::RecordTooLarge {
                                limit: max_record_bytes,
                            });
                        } else {
                            match record_matches_fast(
                                &record,
                                delimiter,
                                row_number,
                                shared.strip_bom_at_zero && record_start == 0,
                                query,
                                &matchers,
                                &groups,
                                max_column,
                                &edits,
                                encoding,
                                &mut field_ranges,
                            ) {
                                Ok(true) => {
                                    pending_matches.push((row_number, record_start));
                                    if pending_matches.len() == MATCH_PUBLISH_BATCH {
                                        publish_matches(shared, &mut pending_matches);
                                    }
                                }
                                Ok(false) => {}
                                Err(error) => deferred_error = Some(error),
                            }
                        }
                    }
                }
            }
            record.clear();
            row_number += 1;
            record_start = absolute_end;
            records_scanned += 1;
            segment_start = local_end;
        });

        absolute_start += read as u64;
        publish_matches(shared, &mut pending_matches);
        shared.bytes_scanned.store(
            absolute_start.saturating_sub(shared.section_base),
            Ordering::Release,
        );
        shared
            .rows_scanned
            .store(records_scanned, Ordering::Release);
        if cancelled || shared.cancel_requested.load(Ordering::Acquire) {
            shared.cancelled.store(true, Ordering::Release);
            return Ok(());
        }
        if let Some(error) = deferred_error {
            return Err(error);
        }
        scan_result?;

        if row_number >= data_start {
            record.extend_from_slice(&chunk[segment_start..read]);
            if record.len() > max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: max_record_bytes,
                });
            }
        }
    }
}

struct FilterRangeOutcome {
    matches: Vec<(u64, u64)>,
    rows: u64,
    end_in_quote: bool,
    open_record_start: Option<u64>,
    error: Option<QuarryError>,
}

struct FilterQuotedOutcome {
    first_record_end: Option<u64>,
    first_error: Option<QuarryError>,
    tail: Option<FilterRangeOutcome>,
}

struct FilterChunkResult {
    id: u64,
    start: u64,
    end: u64,
    is_last: bool,
    field_start: FilterRangeOutcome,
    quoted: Option<FilterQuotedOutcome>,
}

fn find_filter_next_lf(
    file: &File,
    start: u64,
    limit_end: u64,
    cancelled: &AtomicBool,
) -> Result<Option<u64>, QuarryError> {
    let mut pos = start;
    let mut probe = [0u8; FILTER_ALIGNMENT_PROBE_BYTES];
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
fn eval_filter_slice_from_field_start(
    buf: &[u8],
    slice_start: u64,
    end: u64,
    is_last: bool,
    base_row: u64,
    data_start: u64,
    delimiter: u8,
    dialect: ScannerDialect,
    encoding: Encoding,
    byte_class: ByteEncodingClass,
    query: &FilterQuery,
    matchers: &[PredicateMatcher<'_>],
    groups: &[Vec<usize>],
    max_column: usize,
    edits: &FilterEdits,
    max_record_bytes: usize,
    stop_flag: &AtomicBool,
) -> Option<FilterRangeOutcome> {
    let mut scanner = if slice_start == 0 {
        match StructuralScanner::<false>::with_dialect(dialect, 0, byte_class) {
            Ok(scanner) => scanner,
            Err(err) => {
                return Some(FilterRangeOutcome {
                    matches: Vec::new(),
                    rows: 0,
                    end_in_quote: false,
                    open_record_start: None,
                    error: Some(QuarryError::Parse(err)),
                });
            }
        }
    } else {
        match StructuralScanner::<false>::with_dialect_without_bom(dialect, slice_start, byte_class)
        {
            Ok(scanner) => scanner,
            Err(err) => {
                return Some(FilterRangeOutcome {
                    matches: Vec::new(),
                    rows: 0,
                    end_in_quote: false,
                    open_record_start: None,
                    error: Some(QuarryError::Parse(err)),
                });
            }
        }
    };

    let mut matches = Vec::new();
    let mut local_row = 0u64;
    let mut record_start = slice_start;
    let mut field_ranges = Vec::with_capacity((max_column + 1).min(64));
    let mut deferred_error: Option<QuarryError> = None;

    let mut offset = 0usize;
    while offset < buf.len() {
        if stop_flag.load(Ordering::Acquire) {
            return None;
        }
        let sub_end = (offset + FILTER_WORKER_CANCEL_SLICE).min(buf.len());
        let scan_res = scanner.scan_chunk(
            &buf[offset..sub_end],
            slice_start + offset as u64,
            |absolute_end| {
                if deferred_error.is_none() {
                    let row = base_row + local_row;
                    if row >= data_start {
                        let rec_len = (absolute_end - record_start) as usize;
                        if rec_len > max_record_bytes {
                            deferred_error = Some(QuarryError::RecordTooLarge {
                                limit: max_record_bytes,
                            });
                        } else {
                            let s = (record_start - slice_start) as usize;
                            let e = (absolute_end - slice_start) as usize;
                            match record_matches_fast(
                                &buf[s..e],
                                delimiter,
                                row,
                                record_start == 0,
                                query,
                                matchers,
                                groups,
                                max_column,
                                edits,
                                encoding,
                                &mut field_ranges,
                            ) {
                                Ok(true) => matches.push((local_row, record_start)),
                                Ok(false) => {}
                                Err(err) => deferred_error = Some(err),
                            }
                        }
                    }
                }
                local_row += 1;
                record_start = absolute_end;
            },
        );
        if deferred_error.is_some() {
            break;
        }
        if let Err(err) = scan_res {
            deferred_error = Some(QuarryError::Parse(err));
            break;
        }
        offset = sub_end;
    }

    if deferred_error.is_none() && is_last {
        match scanner.finish(end, |_| {}) {
            Ok(true) => {
                let row = base_row + local_row;
                if row >= data_start {
                    let s = (record_start - slice_start) as usize;
                    let rec = &buf[s..];
                    if rec.len() > max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    } else {
                        match record_matches_fast(
                            rec,
                            delimiter,
                            row,
                            record_start == 0,
                            query,
                            matchers,
                            groups,
                            max_column,
                            edits,
                            encoding,
                            &mut field_ranges,
                        ) {
                            Ok(true) => matches.push((local_row, record_start)),
                            Ok(false) => {}
                            Err(err) => deferred_error = Some(err),
                        }
                    }
                }
                local_row += 1;
                record_start = end;
            }
            Ok(false) => {}
            Err(err) => deferred_error = Some(QuarryError::Parse(err)),
        }
    }

    let end_in_quote = scanner.is_in_quote();
    let open_record_start = end_in_quote.then_some(record_start);
    Some(FilterRangeOutcome {
        matches,
        rows: local_row,
        end_in_quote,
        open_record_start,
        error: deferred_error,
    })
}

#[allow(clippy::too_many_arguments)]
fn scan_filter_chunk_worker(
    id: u64,
    start: u64,
    end: u64,
    is_last: bool,
    file: &File,
    delimiter: u8,
    data_start: u64,
    dialect: ScannerDialect,
    encoding: Encoding,
    byte_class: ByteEncodingClass,
    query: &FilterQuery,
    max_record_bytes: usize,
    stop_flag: &AtomicBool,
    #[cfg(test)] delay_chunk_zero: Option<Duration>,
) -> Option<Result<FilterChunkResult, QuarryError>> {
    #[cfg(test)]
    if id == 0
        && let Some(delay) = delay_chunk_zero
    {
        thread::sleep(delay);
    }

    if stop_flag.load(Ordering::Acquire) {
        return None;
    }

    let len = match usize::try_from(end - start) {
        Ok(len) => len,
        Err(_) => return Some(Err(QuarryError::InvalidOption("filter chunk overflow"))),
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

    let matchers = match compile_matchers(query) {
        Ok(m) => m,
        Err(err) => return Some(Err(err)),
    };
    let groups = predicate_groups(query);
    let max_column = query.predicates.iter().map(|p| p.column).max().unwrap_or(0);
    let empty_edits: FilterEdits = Arc::default();

    // Non-initial chunks always contain data rows (>= data_start) because chunk 0 ends after
    // at least one LF (covering row 0 when headered).
    let fs_data_start = if start == 0 { data_start } else { 0 };
    let field_start = eval_filter_slice_from_field_start(
        &buf,
        start,
        end,
        is_last,
        0,
        fs_data_start,
        delimiter,
        dialect,
        encoding,
        byte_class,
        query,
        &matchers,
        &groups,
        max_column,
        &empty_edits,
        max_record_bytes,
        stop_flag,
    )?;

    let quoted = if start == 0 {
        None
    } else if dialect.quote.is_some_and(|q| memchr(q, &buf).is_some()) {
        // Find the end of the incoming open quoted record, then evaluate the remaining slice
        // in-worker so ordered merge only stitches that single boundary record.
        let mut q_scanner = match StructuralScanner::<false>::at_offset_in_quote(
            dialect,
            start,
            byte_class,
            active_simd_backend(),
        ) {
            Ok(scanner) => scanner,
            Err(err) => return Some(Err(QuarryError::Parse(err))),
        };
        let mut first_end: Option<u64> = None;
        let mut first_error: Option<QuarryError> = None;
        let mut pos = 0usize;
        while pos < buf.len() && first_end.is_none() && first_error.is_none() {
            if stop_flag.load(Ordering::Acquire) {
                return None;
            }
            // Scan in small steps until the first record end so we stop immediately once closed.
            let step_end = (pos + 4096).min(buf.len());
            if let Err(err) = q_scanner.scan_chunk(&buf[pos..step_end], start + pos as u64, |re| {
                if first_end.is_none() {
                    first_end = Some(re);
                }
            }) {
                // Only record errors before the incoming record closes; errors after `first_end`
                // belong to the tail and are reported after validating the stitched row.
                if first_end.is_none() {
                    first_error = Some(QuarryError::Parse(err));
                }
            }
            pos = step_end;
        }
        if first_end.is_none() && first_error.is_none() && is_last {
            match q_scanner.finish(end, |_| {}) {
                Ok(true) => first_end = Some(end),
                Ok(false) => {}
                Err(err) => first_error = Some(QuarryError::Parse(err)),
            }
        }
        let tail = if first_error.is_none()
            && let Some(fe) = first_end
            && fe < end
        {
            let tail_slice = &buf[(fe - start) as usize..];
            Some(eval_filter_slice_from_field_start(
                tail_slice,
                fe,
                end,
                is_last,
                0,
                0,
                delimiter,
                dialect,
                encoding,
                byte_class,
                query,
                &matchers,
                &groups,
                max_column,
                &empty_edits,
                max_record_bytes,
                stop_flag,
            )?)
        } else {
            None
        };
        Some(FilterQuotedOutcome {
            first_record_end: first_end,
            first_error,
            tail,
        })
    } else {
        Some(FilterQuotedOutcome {
            first_record_end: None,
            first_error: is_last.then_some(QuarryError::Parse(ParseError {
                offset: end,
                kind: ParseErrorKind::UnterminatedQuote,
            })),
            tail: None,
        })
    };

    Some(Ok(FilterChunkResult {
        id,
        start,
        end,
        is_last,
        field_start,
        quoted,
    }))
}

fn adjust_worker_error(error: QuarryError, base_row: u64) -> QuarryError {
    match error {
        QuarryError::RecordEncoding { record, cause } => QuarryError::RecordEncoding {
            record: base_row.saturating_add(record),
            cause,
        },
        other => other,
    }
}

struct FilterMergeState {
    in_quote: bool,
    open_record_start: u64,
    rows_scanned: u64,
    next_id: u64,
    pending: BTreeMap<u64, Result<FilterChunkResult, QuarryError>>,
}

impl FilterMergeState {
    fn new() -> Self {
        Self {
            in_quote: false,
            open_record_start: 0,
            rows_scanned: 0,
            next_id: 0,
            pending: BTreeMap::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_ready(
        &mut self,
        file: &File,
        delimiter: u8,
        data_start: u64,
        dialect: ScannerDialect,
        encoding: Encoding,
        byte_class: ByteEncodingClass,
        query: &FilterQuery,
        matchers: &[PredicateMatcher<'_>],
        groups: &[Vec<usize>],
        max_column: usize,
        edits: &FilterEdits,
        max_record_bytes: usize,
        stop_flag: &AtomicBool,
        shared: &SharedState,
    ) -> Result<(), QuarryError> {
        let mut publish_buf = Vec::new();
        let mut field_ranges = Vec::with_capacity((max_column + 1).min(64));

        while let Some(result) = self.pending.remove(&self.next_id) {
            self.next_id += 1;
            let chunk = result?;

            if !self.in_quote {
                let base_row = self.rows_scanned;
                let has_edits_in_chunk = !edits.is_empty()
                    && edits
                        .range(
                            (base_row, 0)
                                ..=(base_row.saturating_add(chunk.field_start.rows), usize::MAX),
                        )
                        .next()
                        .is_some();

                if has_edits_in_chunk {
                    let len = usize::try_from(chunk.end - chunk.start)
                        .map_err(|_| QuarryError::InvalidOption("filter chunk overflow"))?;
                    let mut buf = vec![0u8; len];
                    if read_at_exact_or_eof(file, chunk.start, &mut buf)? != len {
                        return Err(QuarryError::SourceChanged);
                    }
                    let Some(exact) = eval_filter_slice_from_field_start(
                        &buf,
                        chunk.start,
                        chunk.end,
                        chunk.is_last,
                        base_row,
                        data_start,
                        delimiter,
                        dialect,
                        encoding,
                        byte_class,
                        query,
                        matchers,
                        groups,
                        max_column,
                        edits,
                        max_record_bytes,
                        stop_flag,
                    ) else {
                        return Ok(());
                    };
                    for (local_row, offset) in exact.matches {
                        publish_buf.push((base_row + local_row, offset));
                    }
                    if let Some(err) = exact.error {
                        publish_matches(shared, &mut publish_buf);
                        return Err(err);
                    }
                    self.rows_scanned += exact.rows;
                    self.in_quote = exact.end_in_quote;
                    if let Some(open) = exact.open_record_start {
                        self.open_record_start = open;
                    }
                } else {
                    for (local_row, offset) in chunk.field_start.matches {
                        publish_buf.push((base_row + local_row, offset));
                    }
                    if let Some(err) = chunk.field_start.error {
                        publish_matches(shared, &mut publish_buf);
                        return Err(adjust_worker_error(err, base_row));
                    }
                    self.rows_scanned += chunk.field_start.rows;
                    self.in_quote = chunk.field_start.end_in_quote;
                    if let Some(open) = chunk.field_start.open_record_start {
                        self.open_record_start = open;
                    }
                }
            } else {
                let q = chunk
                    .quoted
                    .expect("non-initial LF-aligned filter chunk has quoted outcome");
                if let Some(err) = q.first_error {
                    return Err(adjust_worker_error(err, self.rows_scanned));
                }
                if let Some(first_end) = q.first_record_end {
                    let stitched_len = first_end.saturating_sub(self.open_record_start);
                    if stitched_len > max_record_bytes as u64 {
                        return Err(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    }
                    let mut rec = vec![0u8; stitched_len as usize];
                    if read_at_exact_or_eof(file, self.open_record_start, &mut rec)? != rec.len() {
                        return Err(QuarryError::SourceChanged);
                    }
                    let row = self.rows_scanned;
                    if row >= data_start
                        && record_matches_fast(
                            &rec,
                            delimiter,
                            row,
                            self.open_record_start == 0,
                            query,
                            matchers,
                            groups,
                            max_column,
                            edits,
                            encoding,
                            &mut field_ranges,
                        )?
                    {
                        publish_buf.push((row, self.open_record_start));
                    }
                    self.rows_scanned += 1;
                    self.in_quote = false;

                    if let Some(tail) = q.tail {
                        let tail_base = self.rows_scanned;
                        let has_edits_in_tail = !edits.is_empty()
                            && edits
                                .range(
                                    (tail_base, 0)
                                        ..=(tail_base.saturating_add(tail.rows), usize::MAX),
                                )
                                .next()
                                .is_some();
                        if has_edits_in_tail {
                            let len = usize::try_from(chunk.end - first_end)
                                .map_err(|_| QuarryError::InvalidOption("filter chunk overflow"))?;
                            let mut buf = vec![0u8; len];
                            if read_at_exact_or_eof(file, first_end, &mut buf)? != len {
                                return Err(QuarryError::SourceChanged);
                            }
                            let Some(exact) = eval_filter_slice_from_field_start(
                                &buf,
                                first_end,
                                chunk.end,
                                chunk.is_last,
                                tail_base,
                                data_start,
                                delimiter,
                                dialect,
                                encoding,
                                byte_class,
                                query,
                                matchers,
                                groups,
                                max_column,
                                edits,
                                max_record_bytes,
                                stop_flag,
                            ) else {
                                return Ok(());
                            };
                            for (local_row, offset) in exact.matches {
                                publish_buf.push((tail_base + local_row, offset));
                            }
                            if let Some(err) = exact.error {
                                publish_matches(shared, &mut publish_buf);
                                return Err(err);
                            }
                            self.rows_scanned += exact.rows;
                            self.in_quote = exact.end_in_quote;
                            if let Some(open) = exact.open_record_start {
                                self.open_record_start = open;
                            }
                        } else {
                            for (local_row, offset) in tail.matches {
                                publish_buf.push((tail_base + local_row, offset));
                            }
                            if let Some(err) = tail.error {
                                publish_matches(shared, &mut publish_buf);
                                return Err(adjust_worker_error(err, tail_base));
                            }
                            self.rows_scanned += tail.rows;
                            self.in_quote = tail.end_in_quote;
                            if let Some(open) = tail.open_record_start {
                                self.open_record_start = open;
                            }
                        }
                    }
                }
            }

            publish_matches(shared, &mut publish_buf);
            shared.bytes_scanned.store(chunk.end, Ordering::Release);
            shared
                .rows_scanned
                .store(self.rows_scanned, Ordering::Release);
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_filter_giant_line_sequential(
    file: &File,
    start: u64,
    min_stop: u64,
    file_size: u64,
    delimiter: u8,
    data_start: u64,
    dialect: ScannerDialect,
    encoding: Encoding,
    byte_class: ByteEncodingClass,
    query: &FilterQuery,
    matchers: &[PredicateMatcher<'_>],
    groups: &[Vec<usize>],
    max_column: usize,
    edits: &FilterEdits,
    max_record_bytes: usize,
    read_chunk_bytes: usize,
    merge: &mut FilterMergeState,
    shared: &SharedState,
) -> Result<u64, QuarryError> {
    let mut scanner = if start == 0 {
        StructuralScanner::<false>::with_dialect(dialect, 0, byte_class)?
    } else if merge.in_quote {
        StructuralScanner::<false>::at_offset_in_quote(
            dialect,
            start,
            byte_class,
            active_simd_backend(),
        )?
    } else {
        StructuralScanner::<false>::with_dialect_without_bom(dialect, start, byte_class)?
    };

    let mut pos = start;
    let mut record_start = if merge.in_quote {
        merge.open_record_start
    } else {
        start
    };
    let mut buf = vec![0u8; read_chunk_bytes.clamp(64 * 1024, 1024 * 1024)];
    let mut publish_buf = Vec::new();
    let mut field_ranges = Vec::with_capacity((max_column + 1).min(64));

    while pos < file_size {
        if shared.cancel_requested.load(Ordering::Acquire) {
            publish_matches(shared, &mut publish_buf);
            shared.cancelled.store(true, Ordering::Release);
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
        let mut deferred_error: Option<QuarryError> = None;
        let scan_res = scanner.scan_chunk(&buf[..slice_len], pos, |absolute_end| {
            if deferred_error.is_none() {
                let row = merge.rows_scanned;
                if row >= data_start {
                    let rec_len = absolute_end.saturating_sub(record_start);
                    if rec_len > max_record_bytes as u64 {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    } else {
                        let eval_res = if record_start >= pos {
                            let s = (record_start - pos) as usize;
                            let e = (absolute_end - pos) as usize;
                            record_matches_fast(
                                &buf[s..e],
                                delimiter,
                                row,
                                record_start == 0,
                                query,
                                matchers,
                                groups,
                                max_column,
                                edits,
                                encoding,
                                &mut field_ranges,
                            )
                        } else {
                            let mut stitched = vec![0u8; rec_len as usize];
                            match read_at_exact_or_eof(file, record_start, &mut stitched) {
                                Ok(n) if n == stitched.len() => record_matches_fast(
                                    &stitched,
                                    delimiter,
                                    row,
                                    record_start == 0,
                                    query,
                                    matchers,
                                    groups,
                                    max_column,
                                    edits,
                                    encoding,
                                    &mut field_ranges,
                                ),
                                Ok(_) => Err(QuarryError::SourceChanged),
                                Err(err) => Err(QuarryError::Io(err)),
                            }
                        };
                        match eval_res {
                            Ok(true) => publish_buf.push((row, record_start)),
                            Ok(false) => {}
                            Err(err) => deferred_error = Some(err),
                        }
                    }
                }
            }
            merge.rows_scanned += 1;
            record_start = absolute_end;
        });
        if let Some(err) = deferred_error {
            publish_matches(shared, &mut publish_buf);
            return Err(err);
        }
        scan_res?;
        if merge.rows_scanned >= data_start
            && next_pos.saturating_sub(record_start) > max_record_bytes as u64
        {
            publish_matches(shared, &mut publish_buf);
            return Err(QuarryError::RecordTooLarge {
                limit: max_record_bytes,
            });
        }
        if next_pos == file_size && scanner.finish(file_size, |_| {})? {
            let row = merge.rows_scanned;
            if row >= data_start {
                let rec_len = file_size.saturating_sub(record_start);
                if rec_len > max_record_bytes as u64 {
                    return Err(QuarryError::RecordTooLarge {
                        limit: max_record_bytes,
                    });
                }
                let mut stitched = vec![0u8; rec_len as usize];
                if read_at_exact_or_eof(file, record_start, &mut stitched)? != stitched.len() {
                    return Err(QuarryError::SourceChanged);
                }
                if record_matches_fast(
                    &stitched,
                    delimiter,
                    row,
                    record_start == 0,
                    query,
                    matchers,
                    groups,
                    max_column,
                    edits,
                    encoding,
                    &mut field_ranges,
                )? {
                    publish_buf.push((row, record_start));
                }
            }
            merge.rows_scanned += 1;
            record_start = file_size;
        }

        publish_matches(shared, &mut publish_buf);
        pos = next_pos;
        shared.bytes_scanned.store(pos, Ordering::Release);
        shared
            .rows_scanned
            .store(merge.rows_scanned, Ordering::Release);
        if hit_lf_after_min {
            break;
        }
    }

    merge.in_quote = scanner.is_in_quote();
    merge.open_record_start = record_start;
    Ok(pos)
}

#[allow(clippy::too_many_arguments)]
fn run_parallel_filter(
    file: Arc<File>,
    file_size: u64,
    delimiter: u8,
    data_start: u64,
    query: &FilterQuery,
    max_record_bytes: usize,
    encoding: Encoding,
    byte_class: ByteEncodingClass,
    tuning: FilterParallelTuning,
    shared: &SharedState,
) -> Result<(), QuarryError> {
    let edits = shared.index.read().unwrap().edits.clone();
    let dialect = ScannerDialect::rfc4180(delimiter);
    dialect.validate(byte_class)?;
    let matchers = compile_matchers(query)?;
    let groups = predicate_groups(query);
    let max_column = query.predicates.iter().map(|p| p.column).max().unwrap_or(0);

    let chunk_size = tuning.chunk_bytes.max(64);
    let estimated_chunks = file_size.div_ceil(chunk_size as u64).max(1) as usize;
    let effective_workers = tuning.requested_threads.min(estimated_chunks).max(1);
    let pool = engine_pool();
    let executor = JobExecutor::new(pool, JobPriority::Foreground, effective_workers)?;
    let effective_workers = executor.effective_workers();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Result<FilterChunkResult, QuarryError>>();
    let max_window = (effective_workers * 2).clamp(2, 16);
    let query_arc = Arc::new(query.clone());

    let mut merge = FilterMergeState::new();
    let mut cursor = 0u64;
    let mut next_chunk_id = 0u64;

    let run_result: Result<(), QuarryError> = (|| {
        while cursor < file_size {
            if shared.cancel_requested.load(Ordering::Acquire) {
                stop_flag.store(true, Ordering::Release);
                shared.cancelled.store(true, Ordering::Release);
                return Ok(());
            }

            while let Ok(result) = rx.try_recv() {
                match result {
                    Ok(chunk) => {
                        merge.pending.insert(chunk.id, Ok(chunk));
                    }
                    Err(err) => {
                        stop_flag.store(true, Ordering::Release);
                        return Err(err);
                    }
                }
                if let Err(err) = merge.apply_ready(
                    &file,
                    delimiter,
                    data_start,
                    dialect,
                    encoding,
                    byte_class,
                    query,
                    &matchers,
                    &groups,
                    max_column,
                    &edits,
                    max_record_bytes,
                    &stop_flag,
                    shared,
                ) {
                    stop_flag.store(true, Ordering::Release);
                    return Err(err);
                }
            }

            while (next_chunk_id - merge.next_id) as usize >= max_window {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    stop_flag.store(true, Ordering::Release);
                    shared.cancelled.store(true, Ordering::Release);
                    return Ok(());
                }
                match rx.recv_timeout(Duration::from_millis(2)) {
                    Ok(result) => {
                        match result {
                            Ok(chunk) => {
                                merge.pending.insert(chunk.id, Ok(chunk));
                            }
                            Err(err) => {
                                stop_flag.store(true, Ordering::Release);
                                return Err(err);
                            }
                        }
                        if let Err(err) = merge.apply_ready(
                            &file,
                            delimiter,
                            data_start,
                            dialect,
                            encoding,
                            byte_class,
                            query,
                            &matchers,
                            &groups,
                            max_column,
                            &edits,
                            max_record_bytes,
                            &stop_flag,
                            shared,
                        ) {
                            stop_flag.store(true, Ordering::Release);
                            return Err(err);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if executor.has_panicked() {
                            stop_flag.store(true, Ordering::Release);
                            return Err(QuarryError::WorkerPanicked);
                        }
                        continue;
                    }
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
                match find_filter_next_lf(
                    &file,
                    nominal_end,
                    search_limit,
                    &shared.cancel_requested,
                )? {
                    Some(lf_pos) => lf_pos + 1,
                    None if shared.cancel_requested.load(Ordering::Acquire) => {
                        stop_flag.store(true, Ordering::Release);
                        shared.cancelled.store(true, Ordering::Release);
                        return Ok(());
                    }
                    None => {
                        // Drain prior parallel chunks before streaming the giant line sequentially.
                        while merge.next_id < next_chunk_id {
                            if shared.cancel_requested.load(Ordering::Acquire) {
                                stop_flag.store(true, Ordering::Release);
                                shared.cancelled.store(true, Ordering::Release);
                                return Ok(());
                            }
                            match rx.recv_timeout(Duration::from_millis(2)) {
                                Ok(result) => {
                                    match result {
                                        Ok(chunk) => {
                                            merge.pending.insert(chunk.id, Ok(chunk));
                                        }
                                        Err(err) => {
                                            stop_flag.store(true, Ordering::Release);
                                            return Err(err);
                                        }
                                    }
                                    if let Err(err) = merge.apply_ready(
                                        &file,
                                        delimiter,
                                        data_start,
                                        dialect,
                                        encoding,
                                        byte_class,
                                        query,
                                        &matchers,
                                        &groups,
                                        max_column,
                                        &edits,
                                        max_record_bytes,
                                        &stop_flag,
                                        shared,
                                    ) {
                                        stop_flag.store(true, Ordering::Release);
                                        return Err(err);
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {
                                    if executor.has_panicked() {
                                        stop_flag.store(true, Ordering::Release);
                                        return Err(QuarryError::WorkerPanicked);
                                    }
                                    continue;
                                }
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                        cursor = scan_filter_giant_line_sequential(
                            &file,
                            cursor,
                            search_limit,
                            file_size,
                            delimiter,
                            data_start,
                            dialect,
                            encoding,
                            byte_class,
                            query,
                            &matchers,
                            &groups,
                            max_column,
                            &edits,
                            max_record_bytes,
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
                .map_err(|_| QuarryError::InvalidOption("filter chunk overflow"))?;
            let Some(budget_guard) = pool
                .byte_budget()
                .acquire(chunk_len.max(1), &shared.cancel_requested)
            else {
                stop_flag.store(true, Ordering::Release);
                shared.cancelled.store(true, Ordering::Release);
                return Ok(());
            };
            advise_readahead(&file, start, end - start);

            let worker_file = Arc::clone(&file);
            let worker_query = Arc::clone(&query_arc);
            let worker_stop = Arc::clone(&stop_flag);
            let worker_tx = tx.clone();
            #[cfg(test)]
            let delay_zero = tuning.delay_chunk_zero;

            if !executor.spawn(&shared.cancel_requested, move || {
                let _budget = budget_guard;
                let res = scan_filter_chunk_worker(
                    id,
                    start,
                    end,
                    is_last,
                    &worker_file,
                    delimiter,
                    data_start,
                    dialect,
                    encoding,
                    byte_class,
                    &worker_query,
                    max_record_bytes,
                    &worker_stop,
                    #[cfg(test)]
                    delay_zero,
                );
                drop(_budget);
                if let Some(res) = res {
                    let _ = worker_tx.send(res);
                }
            }) {
                stop_flag.store(true, Ordering::Release);
                shared.cancelled.store(true, Ordering::Release);
                return Ok(());
            }
        }

        // Drain all remaining dispatched chunks in strict order.
        while merge.next_id < next_chunk_id {
            if shared.cancel_requested.load(Ordering::Acquire) {
                stop_flag.store(true, Ordering::Release);
                shared.cancelled.store(true, Ordering::Release);
                return Ok(());
            }
            match rx.recv_timeout(Duration::from_millis(2)) {
                Ok(result) => {
                    match result {
                        Ok(chunk) => {
                            merge.pending.insert(chunk.id, Ok(chunk));
                        }
                        Err(err) => {
                            stop_flag.store(true, Ordering::Release);
                            return Err(err);
                        }
                    }
                    if let Err(err) = merge.apply_ready(
                        &file,
                        delimiter,
                        data_start,
                        dialect,
                        encoding,
                        byte_class,
                        query,
                        &matchers,
                        &groups,
                        max_column,
                        &edits,
                        max_record_bytes,
                        &stop_flag,
                        shared,
                    ) {
                        stop_flag.store(true, Ordering::Release);
                        return Err(err);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if executor.has_panicked() {
                        stop_flag.store(true, Ordering::Release);
                        return Err(QuarryError::WorkerPanicked);
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        if !shared.bounded_scan
            && edits
                .keys()
                .next_back()
                .is_some_and(|(row, _)| *row >= merge.rows_scanned)
        {
            return Err(QuarryError::InvalidOption(
                "cell edit points beyond the source rows",
            ));
        }

        Ok(())
    })();

    if run_result.is_err() || shared.cancel_requested.load(Ordering::Acquire) {
        stop_flag.store(true, Ordering::Release);
    }
    drop(tx);
    let drain_res = executor.drain();
    run_result?;
    drain_res?;
    Ok(())
}

fn read_filtered_rows(
    path: &std::path::Path,
    delimiter: u8,
    index: &FilterIndex,
    start_match: u64,
    count: usize,
    max_record_bytes: usize,
) -> Result<Vec<FilterMatch>, QuarryError> {
    let Some(plan) = prepare_filtered_read(index, start_match, count)? else {
        return Ok(Vec::new());
    };
    let file = open_filtered_file(path, plan.checkpoint.offset)?;
    let guard_file = file.try_clone()?;
    if let Some(source) = &plan.source {
        source.check_file(&guard_file)?;
    }
    let outcome = run_filtered_read(
        file,
        delimiter,
        &plan,
        FilterReadConfig {
            chunk_bytes: DEFAULT_READ_CHUNK,
            max_record_bytes,
        },
        None,
    )?;
    if let Some(source) = &plan.source {
        source.check_file(&guard_file)?;
    }
    match outcome {
        FilterReadOutcome::Complete(rows) => Ok(rows),
        FilterReadOutcome::Cancelled => unreachable!("synchronous filtered reads cannot cancel"),
    }
}

fn prepare_filtered_read(
    index: &FilterIndex,
    start_match: u64,
    count: usize,
) -> Result<Option<FilterReadPlan>, QuarryError> {
    if count == 0 {
        return Ok(None);
    }
    if start_match >= index.matches_found() {
        return Err(QuarryError::MatchNotIndexed {
            requested: start_match,
            indexed_matches: index.matches_found(),
        });
    }
    let target_end = start_match
        .saturating_add(count as u64)
        .min(index.matches_found());
    let result_capacity = usize::try_from(target_end - start_match)
        .expect("clamped filtered row count fits in usize");
    let checkpoint = index
        .nearest_checkpoint(start_match)
        .expect("a non-empty filter index retains match zero");
    Ok(Some(FilterReadPlan {
        query: index.query().clone(),
        edits: index.edits.clone(),
        source: index.source.clone(),
        scan_limit: index.scan_limit,
        checkpoint,
        start_match,
        target_end,
        result_capacity,
    }))
}

fn open_filtered_file(path: &std::path::Path, offset: u64) -> Result<File, QuarryError> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    Ok(file)
}

fn filtered_read_cancel_requested(shared: Option<&FilterReadSharedState>) -> bool {
    shared.is_some_and(|shared| shared.cancel_requested.load(Ordering::Acquire))
}

fn publish_filtered_read_progress(
    shared: Option<&FilterReadSharedState>,
    plan: &FilterReadPlan,
    absolute_start: u64,
    rows_scanned: u64,
    matches_read: usize,
) {
    let Some(shared) = shared else {
        return;
    };
    shared.bytes_scanned.store(
        absolute_start.saturating_sub(plan.checkpoint.offset),
        Ordering::Release,
    );
    shared.rows_scanned.store(rows_scanned, Ordering::Release);
    shared
        .matches_read
        .store(matches_read as u64, Ordering::Release);
}

fn run_filtered_read(
    mut file: File,
    delimiter: u8,
    plan: &FilterReadPlan,
    config: FilterReadConfig,
    shared: Option<&FilterReadSharedState>,
) -> Result<FilterReadOutcome, QuarryError> {
    let encoding = plan
        .source
        .as_ref()
        .map_or(Encoding::Utf8, |source| source.encoding);
    let strip_bom_at_zero = plan
        .source
        .as_ref()
        .is_none_or(|source| source.strip_bom_at_zero);
    let matchers = compile_matchers(&plan.query)?;
    let groups = predicate_groups(&plan.query);
    let mut scanner = RecordScanner::at_offset_with_bom_handling(
        delimiter,
        plan.checkpoint.offset,
        encoding,
        strip_bom_at_zero,
    )?;
    let mut chunk = vec![0; encoding.chunk_size(config.chunk_bytes)];
    let mut absolute_start = plan.checkpoint.offset;
    let mut row_number = plan.checkpoint.row;
    let mut record_start = plan.checkpoint.offset;
    let mut match_ordinal = plan.checkpoint.match_ordinal;
    let mut rows_scanned = 0_u64;
    let mut record = Vec::new();
    let mut rows = Vec::with_capacity(plan.result_capacity);

    while match_ordinal < plan.target_end {
        if filtered_read_cancel_requested(shared) {
            return Ok(FilterReadOutcome::Cancelled);
        }
        let read = if let Some(limit) = plan.scan_limit {
            let remaining = limit.saturating_sub(absolute_start);
            if remaining == 0 {
                0
            } else {
                let mut cap = usize::try_from(remaining)
                    .unwrap_or(chunk.len())
                    .min(chunk.len());
                if encoding.is_utf16() {
                    cap &= !1;
                }
                if cap == 0 {
                    0
                } else {
                    encoding.read_chunk(&mut file, &mut chunk[..cap])?
                }
            }
        } else {
            encoding.read_chunk(&mut file, &mut chunk)?
        };
        if read == 0 {
            let mut cancelled = false;
            let has_final_record = scanner.finish(absolute_start, |_| {
                cancelled = filtered_read_cancel_requested(shared);
            })?;
            if has_final_record && !cancelled {
                rows_scanned += 1;
                if record.len() > config.max_record_bytes {
                    return Err(QuarryError::RecordTooLarge {
                        limit: config.max_record_bytes,
                    });
                }
                if let Some(fields) = matching_fields_with_edits(
                    &record,
                    delimiter,
                    row_number,
                    strip_bom_at_zero && record_start == 0,
                    &plan.query,
                    &matchers,
                    &groups,
                    &plan.edits,
                    encoding,
                )? && match_ordinal >= plan.start_match
                {
                    rows.push(FilterMatch {
                        match_ordinal,
                        row: row_number,
                        record_offset: record_start,
                        fields: fields.into_iter().map(Cow::into_owned).collect(),
                    });
                }
            }
            publish_filtered_read_progress(shared, plan, absolute_start, rows_scanned, rows.len());
            return Ok(if cancelled {
                FilterReadOutcome::Cancelled
            } else {
                FilterReadOutcome::Complete(rows)
            });
        }

        let mut segment_start = 0_usize;
        let mut deferred_error = None;
        let mut cancelled = false;
        let scan_result = scanner.scan_chunk(&chunk[..read], absolute_start, |absolute_end| {
            let local_end = (absolute_end - absolute_start) as usize;
            if deferred_error.is_none() && match_ordinal < plan.target_end && !cancelled {
                if filtered_read_cancel_requested(shared) {
                    cancelled = true;
                } else {
                    record.extend_from_slice(&chunk[segment_start..local_end]);
                    if record.len() > config.max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: config.max_record_bytes,
                        });
                    } else {
                        match matching_fields_with_edits(
                            &record,
                            delimiter,
                            row_number,
                            strip_bom_at_zero && record_start == 0,
                            &plan.query,
                            &matchers,
                            &groups,
                            &plan.edits,
                            encoding,
                        ) {
                            Ok(Some(fields)) => {
                                if match_ordinal >= plan.start_match {
                                    rows.push(FilterMatch {
                                        match_ordinal,
                                        row: row_number,
                                        record_offset: record_start,
                                        fields: fields.into_iter().map(Cow::into_owned).collect(),
                                    });
                                }
                                match_ordinal += 1;
                            }
                            Ok(None) => {}
                            Err(error) => deferred_error = Some(error),
                        }
                    }
                }
            }
            record.clear();
            row_number += 1;
            record_start = absolute_end;
            rows_scanned += 1;
            segment_start = local_end;
        });
        absolute_start += read as u64;
        publish_filtered_read_progress(shared, plan, absolute_start, rows_scanned, rows.len());
        scan_result?;
        if let Some(error) = deferred_error {
            return Err(error);
        }
        if match_ordinal >= plan.target_end {
            return Ok(FilterReadOutcome::Complete(rows));
        }
        if cancelled || filtered_read_cancel_requested(shared) {
            return Ok(FilterReadOutcome::Cancelled);
        }
        record.extend_from_slice(&chunk[segment_start..read]);
        if record.len() > config.max_record_bytes {
            return Err(QuarryError::RecordTooLarge {
                limit: config.max_record_bytes,
            });
        }
    }

    Ok(FilterReadOutcome::Complete(rows))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        DEFAULT_FILTER_INDEX_MEMORY_BUDGET_BYTES, DEFAULT_FILTER_SCAN_CONFIG, FilterEdits,
        FilterIndex, FilterJob, FilterOperator, FilterPredicate, FilterQuery, FilterReadCompletion,
        FilterReadConfig, FilterReadJob, FilterReadOutcome, FilterReadSharedState,
        FilterScanConfig, SharedState, WorkerCompletion, prepare_filtered_read,
    };
    use crate::{CaseSensitivity, HeaderMode, OpenOptions, QuarryError, Session};

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture(bytes: &[u8]) -> std::path::PathBuf {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("quarry-filter-{}-{id}.csv", std::process::id()));
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    fn session(path: &std::path::Path, header_mode: HeaderMode) -> Session {
        Session::open(
            path,
            OpenOptions {
                header_mode,
                ..OpenOptions::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn filters_unsaved_snapshot_with_physical_identity_and_raw_edit_baselines() {
        let path = fixture(b"id,state\n1,hit\n2,miss\n3,hit");
        let session = session(&path, HeaderMode::FirstRow);
        let mut values = std::collections::BTreeMap::from([
            ((1, 1), Arc::new(b"miss".to_vec())),
            ((2, 1), Arc::new(b"hit".to_vec())),
        ]);
        let snapshot = Arc::new(values.clone());
        let query = FilterQuery::single(1, FilterOperator::Equals, b"hit".to_vec());
        let index = session
            .start_filter_with_edits(query.clone(), snapshot.clone())
            .unwrap()
            .wait()
            .unwrap();
        // Later changes never alter an in-flight scan or its subsequent page reads.
        values.insert((3, 1), Arc::new(b"miss".to_vec()));
        let rows = session.read_filtered_rows(&index, 0, 10).unwrap();
        assert_eq!(rows.iter().map(|row| row.row).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(rows[0].fields[1], b"miss"); // original value needed for edit/undo
        assert_eq!(rows[1].record_offset, 22);
        let read = session
            .start_filtered_read(&index, 1, 1)
            .unwrap()
            .wait()
            .unwrap();
        let FilterReadOutcome::Complete(rows) = read else {
            panic!("read cancelled");
        };
        assert_eq!(rows[0].row, 3);
        let next = session
            .start_filter_with_edits(query, Arc::new(values))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(next.matches_found(), 1);
        assert!(Arc::ptr_eq(&snapshot, &index.edits));
        assert_eq!(fs::read(&path).unwrap(), b"id,state\n1,hit\n2,miss\n3,hit");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn edited_filters_preserve_exact_numbers_case_and_missing_fields() {
        let path = fixture(b"n,text\n0,old\n1,old\n2,old\n3\n");
        let session = session(&path, HeaderMode::FirstRow);
        let edits = Arc::new(std::collections::BTreeMap::from([
            ((1, 0), Arc::new(b"9007199254740993".to_vec())),
            ((2, 0), Arc::new(b"9007199254740992".to_vec())),
            ((3, 0), Arc::new(b"invalid".to_vec())),
            ((1, 1), Arc::new(b"Hit".to_vec())),
        ]));
        let query =
            FilterQuery::single(0, FilterOperator::GreaterThan, b"9007199254740992".to_vec());
        let index = session
            .start_filter_with_edits(query, edits.clone())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(session.read_filtered_rows(&index, 0, 2).unwrap()[0].row, 1);
        assert_eq!(index.matches_found(), 1);
        let mut query = FilterQuery::single(1, FilterOperator::Equals, b"hit".to_vec());
        query.case_sensitivity = CaseSensitivity::Insensitive;
        assert_eq!(
            session
                .start_filter_with_edits(query.clone(), edits.clone())
                .unwrap()
                .wait()
                .unwrap()
                .matches_found(),
            1
        );
        query.case_sensitivity = CaseSensitivity::Sensitive;
        assert_eq!(
            session
                .start_filter_with_edits(query, edits)
                .unwrap()
                .wait()
                .unwrap()
                .matches_found(),
            0
        );
        let missing = Arc::new(std::collections::BTreeMap::from([(
            (4, 1),
            Arc::new(b"new".to_vec()),
        )]));
        assert!(
            session
                .start_filter_with_edits(
                    FilterQuery::single(1, FilterOperator::Equals, b"new".to_vec()),
                    missing
                )
                .unwrap()
                .wait()
                .is_err()
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn compacted_checkpoints_reread_the_same_edit_snapshot() {
        let bytes = format!(
            "id,state\n{}",
            (1..=100)
                .map(|row| format!("{row},miss\n"))
                .collect::<String>()
        );
        let path = fixture(bytes.as_bytes());
        let session = session(&path, HeaderMode::FirstRow);
        let edits = Arc::new(
            (2..=100)
                .step_by(2)
                .map(|row| ((row, 1), Arc::new(b"hit".to_vec())))
                .collect(),
        );
        let budget = 2 * std::mem::size_of::<super::FilterCheckpoint>();
        let index = FilterJob::start_with_edits(
            path.clone(),
            bytes.len() as u64,
            b',',
            1,
            FilterQuery::single(1, FilterOperator::Equals, b"hit".to_vec()),
            FilterScanConfig {
                chunk_bytes: 5,
                memory_budget_bytes: budget,
                max_record_bytes: 1024,
            },
            edits,
            Some(session.source_stamp.clone()),
            session.encoding,
        )
        .unwrap()
        .wait()
        .unwrap();
        assert_eq!(index.matches_found(), 50);
        assert!(index.memory_bytes() <= budget);
        for ordinal in 0..50 {
            let rows = session.read_filtered_rows(&index, ordinal, 1).unwrap();
            assert_eq!(rows[0].row, (ordinal + 1) * 2);
            assert_eq!(rows[0].fields[1], b"miss");
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filter_snapshots_reject_changed_or_unrelated_sources() {
        let path = fixture(b"x\nhit\n");
        let other = fixture(b"x\nhit\n");
        let session = session(&path, HeaderMode::FirstRow);
        let query = FilterQuery::single(0, FilterOperator::Equals, b"hit".to_vec());
        let index = session.start_filter(query.clone()).unwrap().wait().unwrap();
        assert!(matches!(
            super::Session::open(&other, OpenOptions::default())
                .unwrap()
                .read_filtered_rows(&index, 0, 1),
            Err(QuarryError::SourceChanged)
        ));
        let job = session.start_filtered_read(&index, 0, 1).unwrap();
        while !job.progress().done {
            thread::yield_now();
        }
        fs::write(&path, b"x\nnew\n").unwrap();
        assert!(matches!(job.wait(), Err(QuarryError::SourceChanged)));
        assert!(matches!(
            session.read_filtered_rows(&index, 0, 1),
            Err(QuarryError::SourceChanged)
        ));
        assert!(matches!(
            session.start_filter(query),
            Err(QuarryError::SourceChanged)
        ));
        fs::remove_file(path).unwrap();
        fs::remove_file(other).unwrap();
    }

    #[test]
    fn compacts_match_checkpoints_and_retains_match_zero() {
        let query = FilterQuery::single(0, FilterOperator::Equals, b"hit".to_vec());
        let checkpoint_bytes = std::mem::size_of::<super::FilterCheckpoint>();
        let memory_budget = checkpoint_bytes * 2;
        let mut index = FilterIndex::new(query.clone(), memory_budget).unwrap();
        for value in 0..40 {
            index.record_match(value * 2, value * 10);
        }

        assert_eq!(index.matches_found(), 40);
        assert_eq!(index.query(), &query);
        assert!(index.checkpoints.len() <= 2);
        assert!(index.checkpoints.capacity() <= 2);
        assert_eq!(index.checkpoints[0].match_ordinal, 0);
        assert!(index.checkpoints.iter().all(|checkpoint| {
            checkpoint
                .match_ordinal
                .is_multiple_of(index.checkpoint_every)
        }));
        assert!(index.memory_bytes() <= memory_budget);
        assert!(index.nearest_checkpoint(39).unwrap().match_ordinal <= 39);
    }

    #[test]
    fn filters_decoded_multiline_contains_equals_not_equals_empty_and_ragged_rows() {
        let mut bytes = b"id,note\n1,\"prefix ".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', crate::DEFAULT_READ_CHUNK));
        bytes.extend_from_slice(b"\ntarget \"\"quoted\"\"\"\n2,target\n3,\n4\n5,other");
        let path = fixture(&bytes);
        let session = session(&path, HeaderMode::FirstRow);

        let contains = session
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Contains,
                b"\ntarget \"quoted\"".to_vec(),
            ))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(contains.matches_found(), 1);
        let rows = session.read_filtered_rows(&contains, 0, 2).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].match_ordinal, 0);
        assert_eq!(rows[0].row, 1);
        assert!(rows[0].fields[1].ends_with(b"\ntarget \"quoted\""));

        let equals = session
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"target".to_vec(),
            ))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(session.read_filtered_rows(&equals, 0, 1).unwrap()[0].row, 2);

        let empty = session
            .start_filter(FilterQuery::single(1, FilterOperator::Equals, Vec::new()))
            .unwrap()
            .wait()
            .unwrap();
        let empty_rows = session.read_filtered_rows(&empty, 0, 10).unwrap();
        assert_eq!(
            empty_rows.iter().map(|row| row.row).collect::<Vec<_>>(),
            [3]
        );

        let not_target = session
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::NotEquals,
                b"target".to_vec(),
            ))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            session
                .read_filtered_rows(&not_target, 0, 10)
                .unwrap()
                .iter()
                .map(|row| row.row)
                .collect::<Vec<_>>(),
            [1, 3, 5]
        );

        let not_empty = session
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::NotEquals,
                Vec::new(),
            ))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            session
                .read_filtered_rows(&not_empty, 0, 10)
                .unwrap()
                .iter()
                .map(|row| row.row)
                .collect::<Vec<_>>(),
            [1, 2, 5]
        );

        assert!(matches!(
            session.start_filter(FilterQuery {
                predicates: vec![
                    FilterPredicate {
                        column: 0,
                        operator: FilterOperator::Equals,
                        value: b"1".to_vec(),
                        upper_bound: None,
                    },
                    FilterPredicate {
                        column: 1,
                        operator: FilterOperator::Contains,
                        value: Vec::new(),
                        upper_bound: None,
                    },
                ],
                case_sensitivity: CaseSensitivity::Sensitive,
            }),
            Err(QuarryError::InvalidFilter {
                rule: 2,
                reason: "contains filter value must not be empty",
            })
        ));
        assert!(matches!(
            session.start_filter(FilterQuery {
                predicates: Vec::new(),
                case_sensitivity: CaseSensitivity::Sensitive,
            }),
            Err(QuarryError::InvalidOption(_))
        ));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filters_and_reads_a_headerless_bom_quoted_multiline_first_record() {
        let path = fixture(b"\xEF\xBB\xBF\"one\ncontinued\",x\nother,y\n");
        let session = session(&path, HeaderMode::NoHeader);
        let index = session
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"one\ncontinued".to_vec(),
            ))
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(index.matches_found(), 1);
        let rows = session.read_filtered_rows(&index, 0, 1).unwrap();
        assert_eq!(rows[0].row, 0);
        assert_eq!(rows[0].fields, [b"one\ncontinued".to_vec(), b"x".to_vec()]);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn and_predicates_narrow_multiline_rows_and_range_reads_reuse_query() {
        let path = fixture(
            b"id,status,note\n1,keep,\"line one\nline two\"\n2,keep,other\n3,skip,\"line one\nline two\"\n4,keep,\"line one\nline two\"\n5,keep\n",
        );
        let session = session(&path, HeaderMode::FirstRow);
        let query = FilterQuery {
            predicates: vec![
                FilterPredicate {
                    column: 1,
                    operator: FilterOperator::Equals,
                    value: b"keep".to_vec(),
                    upper_bound: None,
                },
                FilterPredicate {
                    column: 2,
                    operator: FilterOperator::Equals,
                    value: b"line one\nline two".to_vec(),
                    upper_bound: None,
                },
            ],
            case_sensitivity: CaseSensitivity::Sensitive,
        };

        let index = session.start_filter(query.clone()).unwrap().wait().unwrap();
        assert_eq!(index.query(), &query);
        assert_eq!(index.matches_found(), 2);

        let rows = session.read_filtered_rows(&index, 0, 2).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (row.row, row.fields[0].as_slice(), row.fields[2].as_slice()))
                .collect::<Vec<_>>(),
            [
                (1, b"1".as_slice(), b"line one\nline two".as_slice()),
                (4, b"4".as_slice(), b"line one\nline two".as_slice()),
            ]
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn same_column_inclusions_are_alternatives_and_exclusions_accumulate() {
        let path = fixture(
            b"id,state,status\n1,TX,active\n2,FL,active\n3,CA,active\n4,TX,inactive\n5,FL,inactive\n6,,active\n7\n",
        );
        let session = session(&path, HeaderMode::FirstRow);
        let state = |operator, value: &[u8]| FilterPredicate {
            column: 1,
            operator,
            value: value.to_vec(),
            upper_bound: None,
        };
        let matching_ids = |predicates| {
            let index = session
                .start_filter(FilterQuery {
                    predicates,
                    case_sensitivity: CaseSensitivity::Sensitive,
                })
                .unwrap()
                .wait()
                .unwrap();
            session
                .read_filtered_rows(&index, 0, usize::MAX)
                .unwrap()
                .into_iter()
                .map(|row| row.fields[0].clone())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            matching_ids(vec![
                state(FilterOperator::Equals, b"TX"),
                state(FilterOperator::Equals, b"FL"),
                FilterPredicate {
                    column: 2,
                    operator: FilterOperator::Equals,
                    value: b"active".to_vec(),
                    upper_bound: None,
                },
            ]),
            [b"1".to_vec(), b"2".to_vec()]
        );
        assert_eq!(
            matching_ids(vec![
                state(FilterOperator::NotEquals, b"TX"),
                state(FilterOperator::NotEquals, b"FL"),
            ]),
            [b"3".to_vec(), b"6".to_vec()]
        );
        assert_eq!(
            matching_ids(vec![
                state(FilterOperator::Equals, b"TX"),
                state(FilterOperator::Equals, b"FL"),
                state(FilterOperator::NotEquals, b"FL"),
            ]),
            [b"1".to_vec(), b"4".to_vec()]
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn case_insensitive_filter_applies_to_contains_equals_and_exclusions() {
        let path = fixture(b"id,state\n1,TX\n2,tx\n3,Fl\n4,cA\n5,NY\n");
        let session = session(&path, HeaderMode::FirstRow);
        let query = FilterQuery {
            predicates: vec![
                FilterPredicate {
                    column: 1,
                    operator: FilterOperator::Equals,
                    value: b"TX".to_vec(),
                    upper_bound: None,
                },
                FilterPredicate {
                    column: 1,
                    operator: FilterOperator::Contains,
                    value: b"fl".to_vec(),
                    upper_bound: None,
                },
                FilterPredicate {
                    column: 1,
                    operator: FilterOperator::NotEquals,
                    value: b"ca".to_vec(),
                    upper_bound: None,
                },
            ],
            case_sensitivity: CaseSensitivity::Insensitive,
        };

        let index = session.start_filter(query.clone()).unwrap().wait().unwrap();
        assert_eq!(index.query(), &query);
        assert_eq!(index.matches_found(), 3);
        assert_eq!(
            session
                .read_filtered_rows(&index, 0, usize::MAX)
                .unwrap()
                .into_iter()
                .map(|row| row.fields[0].clone())
                .collect::<Vec<_>>(),
            [b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn numeric_filters_use_exact_decoded_numbers_and_inclusive_between() {
        let path = fixture(
            b"\xEF\xBB\xBFid,value\r\nneg,-12.5\r\nneg2,-1e1\r\nzero,-0.00\r\nbelow,9007199254740992\r\nexact,\"\t+9.007199254740993e15\n\"\r\nabove,9007199254740994\r\nfraction,9007199254740993.0000000000000001\r\nblank,\r\nmissing\r\ntext,NaN\r\nseparator,\"1,000\"\r\nhuge,1e1000000\r\ntiny,1e-1000000\r\ninvalid,1e1000001\r\nutf8,\xff\r\nlast,2",
        );
        let session = session(&path, HeaderMode::FirstRow);
        use FilterOperator::*;
        for (operator, lower, upper, expected) in [
            (
                GreaterThan,
                "9007199254740993",
                None,
                vec!["above", "fraction", "huge"],
            ),
            (
                GreaterThanOrEqual,
                "9007199254740993",
                None,
                vec!["exact", "above", "fraction", "huge"],
            ),
            (LessThan, "0", None, vec!["neg", "neg2"]),
            (LessThanOrEqual, "-10", None, vec!["neg", "neg2"]),
            (
                Between,
                "9007199254740993",
                Some("9007199254740994"),
                vec!["exact", "above", "fraction"],
            ),
            (Between, "+0", Some("-0e1000000"), vec!["zero"]),
            (Between, "1e-1000000", Some("1e-1000000"), vec!["tiny"]),
            (Between, "1.999999999999999999999", Some("2"), vec!["last"]),
            (GreaterThan, "1e1000000", None, vec![]),
        ] {
            let mut query = FilterQuery::single(1, operator, lower.as_bytes().to_vec());
            query.predicates[0].upper_bound = upper.map(|value| value.as_bytes().to_vec());
            let index = session.start_filter(query).unwrap().wait().unwrap();
            assert_eq!(
                index.matches_found(),
                expected.len() as u64,
                "{operator:?} {lower}"
            );
            let rows = session
                .read_filtered_rows(&index, 0, expected.len())
                .unwrap();
            assert_eq!(
                rows.iter()
                    .map(|row| std::str::from_utf8(&row.fields[0]).unwrap())
                    .collect::<Vec<_>>(),
                expected,
                "{operator:?} {lower}",
            );
            let outcome = session
                .start_filtered_read(&index, 0, expected.len())
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(outcome, FilterReadOutcome::Complete(rows));
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_numeric_filter_bounds_identify_rule_and_bound() {
        use FilterOperator::*;
        for (operator, value, upper, reason) in [
            (GreaterThan, "", None, "filter value"),
            (LessThan, " \t\r\n", None, "filter value"),
            (GreaterThanOrEqual, "NaN", None, "filter value"),
            (LessThanOrEqual, "1,000", None, "filter value"),
            (GreaterThan, "$12", None, "filter value"),
            (GreaterThan, "1e1000001", None, "filter value"),
            (GreaterThan, "1e-1000001", None, "filter value"),
            (Between, "", Some("1"), "lower bound"),
            (Between, "1", None, "requires an upper bound"),
            (Between, "1", Some(""), "upper bound"),
            (Between, "1", Some("Infinity"), "upper bound"),
            (Between, "2", Some("1"), "less than or equal"),
            (
                Between,
                "9007199254740993",
                Some("9007199254740992"),
                "less than or equal",
            ),
            (Equals, "1", Some("2"), "only Between"),
            (GreaterThan, "1", Some("2"), "only Between"),
        ] {
            let query = FilterQuery {
                predicates: vec![
                    FilterQuery::single(0, Equals, b"ok".to_vec())
                        .predicates
                        .remove(0),
                    FilterPredicate {
                        column: 1,
                        operator,
                        value: value.as_bytes().to_vec(),
                        upper_bound: upper.map(|value| value.as_bytes().to_vec()),
                    },
                ],
                case_sensitivity: CaseSensitivity::Sensitive,
            };
            let error = query.validate().unwrap_err();
            assert!(matches!(error, QuarryError::InvalidFilter { rule: 2, .. }));
            assert!(error.to_string().contains(reason), "{error}");
        }
    }

    #[test]
    fn numeric_grouped_filters_share_bounded_scan_range_read_and_raw_export_results() {
        let bytes = b"\xEF\xBB\xBFid,value,status\r\n1,100,ACTIVE\r\n2,200,active\r\n3,300,active\r\n4,600,active\r\n5,750,inactive\r\n6,700,active\r\n7,garbage,active\r\n8,,active\r\n9\r\n10,250,active\r\n11,50,active\r\n12,350,active";
        let path = fixture(bytes);
        let destination = path.with_extension("export.csv");
        let session = session(&path, HeaderMode::FirstRow);
        let mut between = FilterQuery::single(1, FilterOperator::Between, b"100".to_vec())
            .predicates
            .remove(0);
        between.upper_bound = Some(b"300".to_vec());
        let query = FilterQuery {
            predicates: vec![
                between,
                FilterQuery::single(1, FilterOperator::GreaterThan, b"500".to_vec())
                    .predicates
                    .remove(0),
                FilterQuery::single(1, FilterOperator::Equals, b"garbage".to_vec())
                    .predicates
                    .remove(0),
                FilterQuery::single(1, FilterOperator::NotEquals, b"200".to_vec())
                    .predicates
                    .remove(0),
                FilterQuery::single(1, FilterOperator::NotEquals, b"600".to_vec())
                    .predicates
                    .remove(0),
                FilterQuery::single(2, FilterOperator::Equals, b"active".to_vec())
                    .predicates
                    .remove(0),
            ],
            case_sensitivity: CaseSensitivity::Insensitive,
        };
        let memory_budget_bytes = 2 * std::mem::size_of::<super::FilterCheckpoint>();
        let index = FilterJob::start(
            session.path.clone(),
            session.file_size,
            session.dialect.delimiter,
            1,
            query.clone(),
            FilterScanConfig {
                chunk_bytes: 3,
                memory_budget_bytes,
                max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
            },
        )
        .unwrap()
        .wait()
        .unwrap();
        assert_eq!(index.matches_found(), 5);
        assert!(index.memory_bytes() <= memory_budget_bytes);
        let rows = session.read_filtered_rows(&index, 1, 3).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.fields[0].as_slice())
                .collect::<Vec<_>>(),
            [b"3", b"6", b"7"]
        );
        let outcome = session
            .start_filtered_export(query, &destination)
            .unwrap()
            .wait()
            .unwrap();
        let crate::FilterExportOutcome::Complete(summary) = outcome else {
            panic!("unexpected cancellation")
        };
        assert_eq!(summary.rows_written, 5);
        assert_eq!(fs::read(&destination).unwrap(), b"\xEF\xBB\xBFid,value,status\r\n1,100,ACTIVE\r\n3,300,active\r\n6,700,active\r\n7,garbage,active\r\n10,250,active\r\n");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_file(destination).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reads_incremental_filtered_ranges_from_sparse_checkpoints() {
        let path = fixture(b"value\nmiss\nhit\nmiss\nhit\nhit\nmiss\nhit");
        let session = session(&path, HeaderMode::FirstRow);
        let index = session
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"hit".to_vec(),
            ))
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(index.matches_found(), 4);
        let first = session.read_filtered_rows(&index, 0, 2).unwrap();
        let second = session.read_filtered_rows(&index, 2, 2).unwrap();
        let huge_count = session.read_filtered_rows(&index, 3, usize::MAX).unwrap();
        assert_eq!(
            first
                .iter()
                .chain(&second)
                .map(|found| (found.match_ordinal, found.row, found.fields[0].as_slice()))
                .collect::<Vec<_>>(),
            [
                (0, 2, b"hit".as_slice()),
                (1, 4, b"hit".as_slice()),
                (2, 5, b"hit".as_slice()),
                (3, 7, b"hit".as_slice()),
            ]
        );
        assert_eq!(huge_count.len(), 1);
        assert_eq!(huge_count[0].match_ordinal, 3);
        assert!(matches!(
            session.read_filtered_rows(&index, 4, 1),
            Err(QuarryError::MatchNotIndexed {
                requested: 4,
                indexed_matches: 4
            })
        ));
        assert!(session.read_filtered_rows(&index, 4, 0).unwrap().is_empty());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn background_filtered_read_completes_with_the_requested_rows() {
        let path = fixture(b"value\nmiss\nhit\nmiss\nhit\nhit\nmiss\nhit");
        let session = session(&path, HeaderMode::FirstRow);
        let index = session
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"hit".to_vec(),
            ))
            .unwrap()
            .wait()
            .unwrap();
        let job = session.start_filtered_read(&index, 1, 2).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !job.progress().done {
            assert!(Instant::now() < deadline, "filtered read did not complete");
            thread::yield_now();
        }
        let progress = job.progress();
        assert_eq!(progress.matches_read, 2);
        assert!(progress.rows_scanned > 0);
        assert!(!progress.cancelled);
        let FilterReadOutcome::Complete(rows) = job.wait().unwrap() else {
            panic!("filtered read unexpectedly cancelled");
        };
        assert_eq!(
            rows.iter()
                .map(|found| (found.match_ordinal, found.row))
                .collect::<Vec<_>>(),
            [(1, 4), (2, 5)]
        );
        assert_eq!(rows, session.read_filtered_rows(&index, 1, 2).unwrap());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn background_filtered_read_cancels_while_scanning_a_sparse_gap() {
        for operator in [FilterOperator::Equals, FilterOperator::GreaterThanOrEqual] {
            let mut bytes = b"1\n".repeat(40);
            bytes.extend_from_slice(&b"0\n".repeat(200_000));
            bytes.extend_from_slice(b"1\n");
            let path = fixture(&bytes);
            let session = session(&path, HeaderMode::NoHeader);
            let checkpoint_bytes = std::mem::size_of::<super::FilterCheckpoint>();
            let index = FilterJob::start(
                session.path.clone(),
                session.file_size,
                session.dialect.delimiter,
                0,
                FilterQuery::single(0, operator, b"1".to_vec()),
                FilterScanConfig {
                    chunk_bytes: crate::DEFAULT_READ_CHUNK,
                    memory_budget_bytes: checkpoint_bytes * 2,
                    max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
                },
            )
            .unwrap()
            .wait()
            .unwrap();
            assert_eq!(index.matches_found(), 41);
            let plan = prepare_filtered_read(&index, 40, 1).unwrap().unwrap();
            assert!(plan.checkpoint.match_ordinal < 40);
            let rows_before_gap = 40_u64.saturating_sub(plan.checkpoint.row);
            let job = FilterReadJob::start(
                session.path.clone(),
                session.file_size,
                session.dialect.delimiter,
                Some(plan),
                FilterReadConfig {
                    chunk_bytes: 1,
                    max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
                },
            )
            .unwrap();

            let deadline = Instant::now() + Duration::from_secs(2);
            while job.progress().rows_scanned <= rows_before_gap + 10 {
                assert!(
                    Instant::now() < deadline,
                    "filtered read did not reach the gap"
                );
                thread::yield_now();
            }
            assert_eq!(job.progress().matches_read, 0);
            let shared = Arc::clone(&job.shared);
            job.cancel();

            assert_eq!(job.wait().unwrap(), FilterReadOutcome::Cancelled);
            assert!(shared.done.load(Ordering::Acquire));
            assert!(shared.cancelled.load(Ordering::Acquire));
            assert!(shared.bytes_scanned.load(Ordering::Acquire) < shared.total_bytes);
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn cancellation_returns_a_readable_partial_filter_index() {
        for operator in [FilterOperator::Equals, FilterOperator::GreaterThanOrEqual] {
            let path = fixture(&b"1,value\n".repeat(100_000));
            let session = session(&path, HeaderMode::NoHeader);
            let job = FilterJob::start(
                session.path.clone(),
                session.file_size,
                session.dialect.delimiter,
                0,
                FilterQuery::single(0, operator, b"1".to_vec()),
                FilterScanConfig {
                    chunk_bytes: 1,
                    memory_budget_bytes: 2 * std::mem::size_of::<super::FilterCheckpoint>(),
                    max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
                },
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let progress = job.progress();
                if progress.matches_found >= super::MATCH_PUBLISH_BATCH as u64 {
                    assert!(!progress.done);
                    break;
                }
                assert!(Instant::now() < deadline, "filter did not make progress");
                thread::yield_now();
            }

            job.cancel();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !job.progress().done {
                assert!(Instant::now() < deadline, "filter did not cancel promptly");
                thread::yield_now();
            }
            let progress = job.progress();
            let snapshot = job.snapshot();
            assert!(progress.cancelled);
            assert!(snapshot.memory_bytes() <= 2 * std::mem::size_of::<super::FilterCheckpoint>());
            assert!(snapshot.matches_found() >= super::MATCH_PUBLISH_BATCH as u64);
            assert!(snapshot.matches_found() < 100_000);
            assert_eq!(
                session.read_filtered_rows(&snapshot, 0, 3).unwrap().len(),
                3
            );
            let final_index = job.wait().unwrap();
            assert_eq!(final_index.matches_found(), snapshot.matches_found());
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn filtering_rejects_records_over_the_bounded_cap() {
        for operator in [FilterOperator::Equals, FilterOperator::GreaterThanOrEqual] {
            let path = fixture(b"123456789,tail\n");
            let session = session(&path, HeaderMode::NoHeader);
            let job = FilterJob::start(
                session.path.clone(),
                session.file_size,
                session.dialect.delimiter,
                0,
                FilterQuery::single(0, operator, b"0".to_vec()),
                FilterScanConfig {
                    chunk_bytes: crate::DEFAULT_READ_CHUNK,
                    memory_budget_bytes: DEFAULT_FILTER_INDEX_MEMORY_BUDGET_BYTES,
                    max_record_bytes: 8,
                },
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !job.progress().done {
                assert!(Instant::now() < deadline, "filter did not report its error");
                thread::yield_now();
            }
            assert!(job.error().unwrap().contains("8-byte limit"));
            assert!(matches!(
                job.wait(),
                Err(QuarryError::RecordTooLarge { limit: 8 })
            ));
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn cancelling_after_completion_does_not_relabel_the_scan() {
        let path = fixture(b"value\nhit\nmiss\n");
        let session = session(&path, HeaderMode::FirstRow);
        let job = session
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"hit".to_vec(),
            ))
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !job.progress().done {
            assert!(Instant::now() < deadline, "filter did not complete");
            thread::yield_now();
        }
        assert!(!job.progress().cancelled);

        job.cancel();

        assert!(!job.shared.cancel_requested.load(Ordering::Acquire));
        assert!(!job.progress().cancelled);
        assert_eq!(job.wait().unwrap().matches_found(), 1);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dropping_an_active_filter_cancels_and_joins_its_worker() {
        let index = FilterIndex::new(
            FilterQuery::single(0, FilterOperator::Equals, b"hit".to_vec()),
            DEFAULT_FILTER_INDEX_MEMORY_BUDGET_BYTES,
        )
        .unwrap();
        let shared = Arc::new(SharedState::new(index, 0, 0, false, true));
        let worker_state = Arc::clone(&shared);
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let handle = thread::spawn(move || {
            let _completion = WorkerCompletion(&worker_state);
            let deadline = Instant::now() + Duration::from_secs(2);
            while !worker_state.cancel_requested.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "filter was not cancelled");
                thread::yield_now();
            }
            worker_state.cancelled.store(true, Ordering::Release);
            worker_exited.store(true, Ordering::Release);
            Ok(())
        });
        let job = FilterJob {
            shared: Arc::clone(&shared),
            handle: Some(handle),
        };

        drop(job);

        assert!(shared.cancel_requested.load(Ordering::Acquire));
        assert!(shared.cancelled.load(Ordering::Acquire));
        assert!(shared.done.load(Ordering::Acquire));
        assert!(exited.load(Ordering::Acquire));
    }

    #[test]
    fn dropping_an_active_filtered_read_cancels_and_joins_its_worker() {
        let shared = Arc::new(FilterReadSharedState::new(100));
        let worker_state = Arc::clone(&shared);
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let handle = thread::spawn(move || {
            let _completion = FilterReadCompletion(&worker_state);
            let deadline = Instant::now() + Duration::from_secs(2);
            while !worker_state.cancel_requested.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "filtered read was not cancelled");
                thread::yield_now();
            }
            worker_state.cancelled.store(true, Ordering::Release);
            worker_exited.store(true, Ordering::Release);
            Ok(FilterReadOutcome::Cancelled)
        });
        let job = FilterReadJob {
            source: None,
            shared: Arc::clone(&shared),
            handle: Some(handle),
        };

        drop(job);

        assert!(shared.cancel_requested.load(Ordering::Acquire));
        assert!(shared.cancelled.load(Ordering::Acquire));
        assert!(shared.done.load(Ordering::Acquire));
        assert!(exited.load(Ordering::Acquire));
    }

    #[test]
    fn cancelling_a_filtered_read_without_waiting_does_not_wait_for_exit() {
        let shared = Arc::new(FilterReadSharedState::new(100));
        let worker_state = Arc::clone(&shared);
        let release = Arc::new(AtomicBool::new(false));
        let worker_release = Arc::clone(&release);
        let handle = thread::spawn(move || {
            let _completion = FilterReadCompletion(&worker_state);
            let deadline = Instant::now() + Duration::from_secs(2);
            while !worker_state.cancel_requested.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "filtered read was not cancelled");
                thread::yield_now();
            }
            while !worker_release.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "filtered read was not released");
                thread::yield_now();
            }
            worker_state.cancelled.store(true, Ordering::Release);
            Ok(FilterReadOutcome::Cancelled)
        });
        let job = FilterReadJob {
            source: None,
            shared: Arc::clone(&shared),
            handle: Some(handle),
        };

        job.cancel_without_waiting();

        assert!(shared.cancel_requested.load(Ordering::Acquire));
        assert!(!shared.done.load(Ordering::Acquire));
        release.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !shared.done.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "detached filtered read did not finish"
            );
            thread::yield_now();
        }
        assert!(shared.cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn parallel_filter_matches_sequential_across_multiline_quotes_edits_and_slow_first_chunk() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("parallel-filter.csv");
        let mut data = Vec::new();
        data.extend_from_slice(b"\xEF\xBB\xBFid,score,notes\n");
        for i in 1..=300_u32 {
            if i % 9 == 0 {
                data.extend_from_slice(
                    format!("{i},{},\"multi\nline\n\"\"quoted\"\"\n{i}\"\r\n", i % 50).as_bytes(),
                );
            } else {
                data.extend_from_slice(format!("{i},{},plain_{i}\n", i % 50).as_bytes());
            }
        }
        data.extend_from_slice(b"301,45,\"trailing\nwithout_lf\"");
        fs::write(&path, &data).unwrap();

        let session = Session::open(&path, OpenOptions::default()).unwrap();
        let query = FilterQuery {
            predicates: vec![
                FilterPredicate {
                    column: 1,
                    operator: FilterOperator::GreaterThan,
                    value: b"35".to_vec(),
                    upper_bound: None,
                },
                FilterPredicate {
                    column: 2,
                    operator: FilterOperator::Contains,
                    value: b"0".to_vec(),
                    upper_bound: None,
                },
            ],
            case_sensitivity: CaseSensitivity::Sensitive,
        };
        let mut edit_map = BTreeMap::new();
        edit_map.insert((5_u64, 1usize), Arc::new(b"49".to_vec()));
        edit_map.insert((5_u64, 2usize), Arc::new(b"forced_0_hit".to_vec()));
        edit_map.insert((40_u64, 1usize), Arc::new(b"1".to_vec()));
        let edits: FilterEdits = Arc::new(edit_map);

        let sequential = session
            .start_filter_with_edits_and_threads(query.clone(), Arc::clone(&edits), Some(1))
            .unwrap()
            .wait()
            .unwrap();
        let seq_rows = session
            .read_filtered_rows(&sequential, 0, sequential.matches_found() as usize)
            .unwrap();

        let parallel = FilterJob::start_with_edits_and_tuning(
            session.path().to_path_buf(),
            session.file_size,
            session.dialect.delimiter,
            u64::from(session.dialect.has_header),
            query,
            DEFAULT_FILTER_SCAN_CONFIG,
            edits,
            Some(session.source_stamp.clone()),
            session.encoding,
            super::FilterParallelTuning {
                requested_threads: 4,
                min_parallel_bytes: 64,
                chunk_bytes: 256,
                max_alignment_search: 256,
                delay_chunk_zero: Some(Duration::from_millis(25)),
            },
            false,
        )
        .unwrap()
        .wait()
        .unwrap();
        let par_rows = session
            .read_filtered_rows(&parallel, 0, parallel.matches_found() as usize)
            .unwrap();

        assert_eq!(parallel.matches_found(), sequential.matches_found());
        assert_eq!(par_rows, seq_rows);
    }

    #[test]
    fn parallel_filter_preceding_parse_error_wins_over_later_matches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("filter-malformed.csv");
        let mut data = Vec::with_capacity(1_300_000);
        data.extend_from_slice(b"id,val\n");
        for i in 1..=65_000_u32 {
            data.extend_from_slice(format!("{i},ok_row_{i:05}\n").as_bytes());
        }
        data.extend_from_slice(b"65001,\"bad\"after\n");
        for i in 65_002..=66_000_u32 {
            data.extend_from_slice(format!("{i},target_hit_{i}\n").as_bytes());
        }
        fs::write(&path, &data).unwrap();

        let session = Session::open(
            &path,
            OpenOptions {
                rows: 5,
                delimiter: Some(b','),
                sample_bytes: 256,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let query = FilterQuery::single(1, FilterOperator::Contains, b"target_hit".to_vec());
        let seq_err = session
            .start_filter_with_edits_and_threads(query.clone(), Arc::default(), Some(1))
            .unwrap()
            .wait()
            .unwrap_err();
        let par_err = FilterJob::start_with_edits_and_tuning(
            session.path().to_path_buf(),
            session.file_size,
            session.dialect.delimiter,
            u64::from(session.dialect.has_header),
            query,
            DEFAULT_FILTER_SCAN_CONFIG,
            Arc::default(),
            Some(session.source_stamp.clone()),
            session.encoding,
            super::FilterParallelTuning {
                requested_threads: 4,
                min_parallel_bytes: 64,
                chunk_bytes: 64 * 1024,
                max_alignment_search: 8 * 1024,
                delay_chunk_zero: Some(Duration::from_millis(15)),
            },
            false,
        )
        .unwrap()
        .wait()
        .unwrap_err();

        match (seq_err, par_err) {
            (QuarryError::Parse(a), QuarryError::Parse(b)) => assert_eq!(a, b),
            (a, b) => panic!("expected matching Parse errors, got seq={a:?} par={b:?}"),
        }
    }

    #[test]
    fn parallel_filter_preserves_leading_utf8_bom_bytes_in_non_initial_chunks() {
        let mut data = Vec::new();
        data.extend_from_slice(b"name,val\n");
        for i in 1..=200_u32 {
            data.extend_from_slice(format!("plain_{i:04},x\n").as_bytes());
        }
        // Place records with literal leading U+FEFF across several small chunks so at least one
        // lands immediately after a chunk boundary.
        for i in 0..40_u32 {
            data.extend_from_slice(b"\xEF\xBB\xBFtarget,");
            data.extend_from_slice(format!("{i}\n").as_bytes());
        }
        let path = fixture(&data);
        let session = session(&path, HeaderMode::FirstRow);

        for needle in [b"target".as_slice(), b"\xEF\xBB\xBFtarget".as_slice()] {
            let query = FilterQuery::single(0, FilterOperator::Equals, needle.to_vec());
            let seq = session
                .start_filter_with_edits_and_threads(query.clone(), Arc::default(), Some(1))
                .unwrap()
                .wait()
                .unwrap();
            let par = FilterJob::start_with_edits_and_tuning(
                session.path().to_path_buf(),
                session.file_size,
                session.dialect.delimiter,
                u64::from(session.dialect.has_header),
                query,
                DEFAULT_FILTER_SCAN_CONFIG,
                Arc::default(),
                Some(session.source_stamp.clone()),
                session.encoding,
                super::FilterParallelTuning {
                    requested_threads: 4,
                    min_parallel_bytes: 64,
                    chunk_bytes: 128,
                    max_alignment_search: 256,
                    delay_chunk_zero: None,
                },
                false,
            )
            .unwrap()
            .wait()
            .unwrap();
            assert_eq!(par.matches_found(), seq.matches_found());
            if needle == b"target" {
                assert_eq!(par.matches_found(), 0);
            } else {
                assert_eq!(par.matches_found(), 40);
            }
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filter_handles_huge_column_index_without_large_allocation() {
        let path = fixture(b"a,b\n1,2\n3,4\n");
        let session = session(&path, HeaderMode::FirstRow);
        let query = FilterQuery::single(1_000_000_000, FilterOperator::Equals, b"1".to_vec());
        let index = session
            .start_filter_with_edits_and_threads(query, Arc::default(), Some(1))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(index.matches_found(), 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn bounded_filter_reads_stop_at_scan_limit_and_ignore_edits_beyond_frontier() {
        let mut prefix = Vec::from(&b"id,val\n1,ok\n2,ok\n"[..]);
        for i in 3..=49 {
            prefix.extend_from_slice(format!("{i},ok\n").as_bytes());
        }
        prefix.extend_from_slice(b"50,hit\n");
        let malformed_suffix = b"51,\"bad\"trailing\n";
        let mut data = Vec::with_capacity(prefix.len() + malformed_suffix.len());
        data.extend_from_slice(&prefix);
        data.extend_from_slice(malformed_suffix);
        let path = fixture(&data);
        let session = Session::open(
            &path,
            OpenOptions {
                rows: 2,
                delimiter: Some(b','),
                header_mode: HeaderMode::FirstRow,
                sample_bytes: 128,
                bootstrap_limit: 128,
            },
        )
        .unwrap();

        let mut edits = BTreeMap::new();
        edits.insert((99_999, 1), Arc::new(b"beyond_frontier_edit".to_vec()));
        let query = FilterQuery::single(1, FilterOperator::Equals, b"hit".to_vec());
        let index = session
            .start_filter_with_edits_and_limit(query, Arc::new(edits), Some(prefix.len() as u64))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(index.matches_found(), 1);
        let rows = session.read_filtered_rows(&index, 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row, 50);
        let async_rows = match session
            .start_filtered_read(&index, 0, 10)
            .unwrap()
            .wait()
            .unwrap()
        {
            super::FilterReadOutcome::Complete(rows) => rows,
            super::FilterReadOutcome::Cancelled => panic!("unexpected cancel"),
        };
        assert_eq!(async_rows, rows);
        fs::remove_file(path).unwrap();
    }
}
