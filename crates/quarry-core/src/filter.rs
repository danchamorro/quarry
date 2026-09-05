use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use quarry_delimited::RecordScanner;

use crate::case::ByteMatcher;
use crate::sort::numeric_key;
use crate::{
    CaseSensitivity, DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, QuarryError, Session,
    parse_source_record,
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
    compile_matchers(query).map(|_| ())
}

pub(crate) enum PredicateMatcher<'a> {
    Text(ByteMatcher<'a>),
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
                    return Err(QuarryError::InvalidOption(
                        "contains filter value must not be empty",
                    ));
                }
                return Ok(PredicateMatcher::Text(ByteMatcher::new(
                    &predicate.value,
                    query.case_sensitivity,
                )));
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

pub(crate) fn matching_fields<'a>(
    record: &'a [u8],
    delimiter: u8,
    physical_row: u64,
    query: &FilterQuery,
    matchers: &[PredicateMatcher<'_>],
    groups: &[Vec<usize>],
) -> Result<Option<Vec<Cow<'a, [u8]>>>, QuarryError> {
    let fields = parse_source_record(record, delimiter, physical_row)?;
    debug_assert_eq!(query.predicates.len(), matchers.len());
    let matches = groups.iter().all(|group| {
        let Some(&first) = group.first() else {
            return true;
        };
        let Some(field) = fields.get(query.predicates[first].column) else {
            return false;
        };
        let field: &[u8] = field.as_ref();
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
    let matchers = compile_matchers(query)?;
    let groups = predicate_groups(query);
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
                        match matching_fields(
                            &record, delimiter, row_number, query, &matchers, &groups,
                        ) {
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
                        match matching_fields(
                            &record, delimiter, row_number, query, &matchers, &groups,
                        ) {
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
    let matchers = compile_matchers(&plan.query)?;
    let groups = predicate_groups(&plan.query);
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
                if let Some(fields) = matching_fields(
                    &record,
                    delimiter,
                    row_number,
                    &plan.query,
                    &matchers,
                    &groups,
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
                        match matching_fields(
                            &record,
                            delimiter,
                            row_number,
                            &plan.query,
                            &matchers,
                            &groups,
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
            Err(QuarryError::InvalidOption(_))
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
