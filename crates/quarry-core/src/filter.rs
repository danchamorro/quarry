use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use memchr::memmem::Finder;
use quarry_delimited::RecordScanner;

use crate::{
    DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, QuarryError, Session, parse_source_record,
};

const DEFAULT_FILTER_INDEX_MEMORY_BUDGET_BYTES: usize = 16 * 1024 * 1024;
const MATCH_PUBLISH_BATCH: usize = 256;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterPredicate {
    pub column: usize,
    pub operator: FilterOperator,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterQuery {
    pub predicates: Vec<FilterPredicate>,
}

impl FilterQuery {
    pub fn single(column: usize, operator: FilterOperator, value: Vec<u8>) -> Self {
        Self {
            predicates: vec![FilterPredicate {
                column,
                operator,
                value,
            }],
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
    checkpoint: FilterCheckpoint,
    start_match: u64,
    target_end: u64,
    result_capacity: usize,
}

#[derive(Debug, Clone)]
pub struct FilterIndex {
    query: FilterQuery,
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
            checkpoints: Vec::with_capacity(max_checkpoints.min(4)),
            checkpoint_every: 1,
            max_checkpoints,
            matches_found: 0,
        })
    }

    fn record_match(&mut self, row: u64, offset: u64) {
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
    file_size: u64,
}

impl SharedState {
    fn new(index: FilterIndex, file_size: u64) -> Self {
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
            file_size,
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
    fn start(
        path: PathBuf,
        file_size: u64,
        delimiter: u8,
        data_start: u64,
        query: FilterQuery,
        config: FilterScanConfig,
    ) -> Result<Self, QuarryError> {
        if config.chunk_bytes == 0 {
            return Err(QuarryError::InvalidOption(
                "filter scan chunk must be non-zero",
            ));
        }
        validate_query(&query)?;
        let index = FilterIndex::new(query.clone(), config.memory_budget_bytes)?;
        let file = File::open(path)?;
        let shared = Arc::new(SharedState::new(index, file_size));
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-filter".into())
            .spawn(move || {
                let _completion = WorkerCompletion(&worker_state);
                let result = run_filter(
                    file,
                    delimiter,
                    data_start,
                    &query,
                    config.chunk_bytes,
                    config.max_record_bytes,
                    &worker_state,
                );
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
        Ok(self.shared.index.read().unwrap().clone())
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
        let total_bytes = plan
            .as_ref()
            .map_or(0, |plan| file_size.saturating_sub(plan.checkpoint.offset));
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
                if matches!(&result, Ok(FilterReadOutcome::Cancelled)) {
                    worker_state.cancelled.store(true, Ordering::Release);
                }
                result
            })?;
        Ok(Self {
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
        self.handle
            .take()
            .expect("filter read handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
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
        FilterJob::start(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            u64::from(self.dialect.has_header),
            query,
            DEFAULT_FILTER_SCAN_CONFIG,
        )
    }

    pub fn read_filtered_rows(
        &self,
        index: &FilterIndex,
        start_match: u64,
        count: usize,
    ) -> Result<Vec<FilterMatch>, QuarryError> {
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
        let plan = prepare_filtered_read(index, start_match, count)?;
        FilterReadJob::start(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            plan,
            DEFAULT_FILTER_READ_CONFIG,
        )
    }
}

pub(crate) fn validate_query(query: &FilterQuery) -> Result<(), QuarryError> {
    if query.predicates.is_empty() {
        return Err(QuarryError::InvalidOption(
            "filter query must contain at least one predicate",
        ));
    }
    if query.predicates.iter().any(|predicate| {
        predicate.operator == FilterOperator::Contains && predicate.value.is_empty()
    }) {
        return Err(QuarryError::InvalidOption(
            "contains filter value must not be empty",
        ));
    }
    Ok(())
}

pub(crate) fn matching_fields<'a>(
    record: &'a [u8],
    delimiter: u8,
    physical_row: u64,
    query: &FilterQuery,
    finders: &[Finder<'_>],
) -> Result<Option<Vec<Cow<'a, [u8]>>>, QuarryError> {
    let fields = parse_source_record(record, delimiter, physical_row)?;
    debug_assert_eq!(query.predicates.len(), finders.len());
    let matches = query
        .predicates
        .iter()
        .zip(finders)
        .all(|(predicate, finder)| {
            let Some(field) = fields.get(predicate.column) else {
                return false;
            };
            match predicate.operator {
                FilterOperator::Contains => finder.find(field.as_ref()).is_some(),
                FilterOperator::Equals => field.as_ref() == predicate.value.as_slice(),
                FilterOperator::NotEquals => field.as_ref() != predicate.value.as_slice(),
            }
        });
    Ok(matches.then_some(fields))
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
    let finders: Vec<_> = query
        .predicates
        .iter()
        .map(|predicate| Finder::new(&predicate.value))
        .collect();
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut chunk = vec![0; chunk_bytes];
    let mut absolute_start = 0_u64;
    let mut row_number = 0_u64;
    let mut record_start = 0_u64;
    let mut records_scanned = 0_u64;
    let mut record = Vec::new();
    let mut pending_matches = Vec::with_capacity(MATCH_PUBLISH_BATCH);

    loop {
        if shared.cancel_requested.load(Ordering::Acquire) {
            publish_matches(shared, &mut pending_matches);
            shared.cancelled.store(true, Ordering::Release);
            return Ok(());
        }
        let read = file.read(&mut chunk)?;
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
                        match matching_fields(&record, delimiter, row_number, query, &finders) {
                            Ok(Some(_)) => pending_matches.push((row_number, record_start)),
                            Ok(None) => {}
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
            shared
                .bytes_scanned
                .store(absolute_start, Ordering::Release);
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
                    record.extend_from_slice(&chunk[segment_start..local_end]);
                    if record.len() > max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    } else {
                        match matching_fields(&record, delimiter, row_number, query, &finders) {
                            Ok(Some(_)) => {
                                pending_matches.push((row_number, record_start));
                                if pending_matches.len() == MATCH_PUBLISH_BATCH {
                                    publish_matches(shared, &mut pending_matches);
                                }
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
            records_scanned += 1;
            segment_start = local_end;
        });

        absolute_start += read as u64;
        publish_matches(shared, &mut pending_matches);
        shared
            .bytes_scanned
            .store(absolute_start, Ordering::Release);
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
    match run_filtered_read(
        file,
        delimiter,
        &plan,
        FilterReadConfig {
            chunk_bytes: DEFAULT_READ_CHUNK,
            max_record_bytes,
        },
        None,
    )? {
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
    let finders: Vec<_> = plan
        .query
        .predicates
        .iter()
        .map(|predicate| Finder::new(&predicate.value))
        .collect();
    let mut scanner = RecordScanner::at_offset(delimiter, plan.checkpoint.offset)?;
    let mut chunk = vec![0; config.chunk_bytes];
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
        let read = file.read(&mut chunk)?;
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
                if let Some(fields) =
                    matching_fields(&record, delimiter, row_number, &plan.query, &finders)?
                    && match_ordinal >= plan.start_match
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
                        match matching_fields(&record, delimiter, row_number, &plan.query, &finders)
                        {
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
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        DEFAULT_FILTER_INDEX_MEMORY_BUDGET_BYTES, FilterIndex, FilterJob, FilterOperator,
        FilterPredicate, FilterQuery, FilterReadCompletion, FilterReadConfig, FilterReadJob,
        FilterReadOutcome, FilterReadSharedState, FilterScanConfig, SharedState, WorkerCompletion,
        prepare_filtered_read,
    };
    use crate::{HeaderMode, OpenOptions, QuarryError, Session};

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
                    },
                    FilterPredicate {
                        column: 1,
                        operator: FilterOperator::Contains,
                        value: Vec::new(),
                    },
                ],
            }),
            Err(QuarryError::InvalidOption(_))
        ));
        assert!(matches!(
            session.start_filter(FilterQuery {
                predicates: Vec::new(),
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
                },
                FilterPredicate {
                    column: 2,
                    operator: FilterOperator::Equals,
                    value: b"line one\nline two".to_vec(),
                },
            ],
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
        let mut bytes = b"hit\n".repeat(40);
        bytes.extend_from_slice(&b"miss\n".repeat(200_000));
        bytes.extend_from_slice(b"hit\n");
        let path = fixture(&bytes);
        let session = session(&path, HeaderMode::NoHeader);
        let checkpoint_bytes = std::mem::size_of::<super::FilterCheckpoint>();
        let index = FilterJob::start(
            session.path.clone(),
            session.file_size,
            session.dialect.delimiter,
            0,
            FilterQuery::single(0, FilterOperator::Equals, b"hit".to_vec()),
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

    #[test]
    fn cancellation_returns_a_readable_partial_filter_index() {
        let path = fixture(&b"hit,value\n".repeat(100_000));
        let session = session(&path, HeaderMode::NoHeader);
        let job = FilterJob::start(
            session.path.clone(),
            session.file_size,
            session.dialect.delimiter,
            0,
            FilterQuery::single(0, FilterOperator::Equals, b"hit".to_vec()),
            FilterScanConfig {
                chunk_bytes: 1,
                memory_budget_bytes: DEFAULT_FILTER_INDEX_MEMORY_BUDGET_BYTES,
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

    #[test]
    fn filtering_rejects_records_over_the_bounded_cap() {
        let path = fixture(b"123456789,tail\n");
        let session = session(&path, HeaderMode::NoHeader);
        let job = FilterJob::start(
            session.path.clone(),
            session.file_size,
            session.dialect.delimiter,
            0,
            FilterQuery::single(0, FilterOperator::Equals, b"missing".to_vec()),
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
        let shared = Arc::new(SharedState::new(index, 0));
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
}
