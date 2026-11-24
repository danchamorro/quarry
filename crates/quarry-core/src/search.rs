use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::Encoding;
use crate::case::ByteMatcher;
use crate::encoding::RecordScanner;
use crate::pool::{JobExecutor, JobPriority, engine_pool, resolve_worker_count};
use crate::positional_io::{advise_readahead, read_at_exact_or_eof};
use crate::source_stamp::SourceStamp;
use crate::{
    CaseSensitivity, Checkpoint, DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, QuarryError,
    Session, StructuralIndex,
};

const DEFAULT_SEARCH_STRIPE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SINGLE_READ_STRIPE_BYTES: u64 = 8 * 1024 * 1024;
const WORKER_CANCEL_SLICE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchPosition {
    pub row: u64,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    pub row: u64,
    pub column: usize,
    pub record_offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOutcome {
    Match(SearchMatch),
    NotFound,
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchProgress {
    pub bytes_scanned: u64,
    pub rows_scanned: u64,
    pub total_bytes: u64,
    pub elapsed: Duration,
    pub done: bool,
    pub cancelled: bool,
}

/// Telemetry reported by literal search jobs (including bounded speculative work).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchTelemetry {
    pub effective_workers: usize,
    pub wasted_work_bound_bytes: u64,
    pub stripes_scanned: u64,
}

impl Default for SearchTelemetry {
    fn default() -> Self {
        Self {
            effective_workers: 1,
            wasted_work_bound_bytes: 0,
            stripes_scanned: 0,
        }
    }
}

struct SharedState {
    bytes_scanned: AtomicU64,
    rows_scanned: AtomicU64,
    finished_nanos: AtomicU64,
    done: AtomicBool,
    cancelled: AtomicBool,
    error: Mutex<Option<String>>,
    telemetry: Mutex<SearchTelemetry>,
    started: Instant,
    total_bytes: u64,
}

impl SharedState {
    fn new(total_bytes: u64) -> Self {
        Self {
            bytes_scanned: AtomicU64::new(0),
            rows_scanned: AtomicU64::new(0),
            finished_nanos: AtomicU64::new(0),
            done: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            error: Mutex::new(None),
            telemetry: Mutex::new(SearchTelemetry::default()),
            started: Instant::now(),
            total_bytes,
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

#[derive(Debug, Clone, Copy)]
struct SearchStripe {
    id: u64,
    start_row: u64,
    start_offset: u64,
    end_offset: u64,
    is_eof: bool,
}

#[derive(Debug, Clone, Copy)]
struct SearchTuning {
    requested_threads: usize,
    stripe_target_bytes: u64,
    #[cfg(test)]
    wait_for_later_hit_before_stripe_zero: bool,
}

impl SearchTuning {
    fn for_production(requested_threads: usize) -> Self {
        Self {
            requested_threads,
            stripe_target_bytes: DEFAULT_SEARCH_STRIPE_BYTES,
            #[cfg(test)]
            wait_for_later_hit_before_stripe_zero: false,
        }
    }
}

pub struct SearchJob {
    shared: Arc<SharedState>,
    handle: Option<JoinHandle<Result<SearchOutcome, QuarryError>>>,
}

impl SearchJob {
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn start(
        path: PathBuf,
        file_size: u64,
        delimiter: u8,
        needle: Vec<u8>,
        case_sensitivity: CaseSensitivity,
        start: SearchPosition,
        checkpoint: Checkpoint,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        max_record_bytes: usize,
        encoding: Encoding,
    ) -> Result<Self, QuarryError> {
        let stripes = vec![SearchStripe {
            id: 0,
            start_row: checkpoint.row,
            start_offset: checkpoint.offset,
            end_offset: file_size,
            is_eof: true,
        }];
        Self::start_plan(
            path,
            file_size,
            file_size,
            0,
            None,
            delimiter,
            needle,
            case_sensitivity,
            start,
            checkpoint,
            stripes,
            cell_edits,
            max_record_bytes,
            encoding,
            SearchTuning::for_production(1),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_plan(
        path: PathBuf,
        file_size: u64,
        indexed_frontier: u64,
        indexed_rows: u64,
        expected_source: Option<SourceStamp>,
        delimiter: u8,
        needle: Vec<u8>,
        case_sensitivity: CaseSensitivity,
        start: SearchPosition,
        checkpoint: Checkpoint,
        stripes: Vec<SearchStripe>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        max_record_bytes: usize,
        encoding: Encoding,
        tuning: SearchTuning,
    ) -> Result<Self, QuarryError> {
        if cell_edits
            .values()
            .any(|value| value.len() > max_record_bytes)
        {
            return Err(QuarryError::RecordTooLarge {
                limit: max_record_bytes,
            });
        }
        let mut file = File::open(&path)?;
        if let Some(ref stamp) = expected_source
            && !crate::export::source_matches_stamp(&file, &path, stamp)?
        {
            return Err(QuarryError::SourceChanged);
        }
        let guard_file = expected_source
            .as_ref()
            .map(|_| file.try_clone())
            .transpose()?;
        file.seek(SeekFrom::Start(checkpoint.offset))?;
        let total_bytes = indexed_frontier.saturating_sub(checkpoint.offset);
        let shared = Arc::new(SharedState::new(total_bytes));
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-search".into())
            .spawn(move || {
                let _completion = WorkerCompletion(&worker_state);
                let budget_cap = engine_pool().byte_budget().limit() as u64;
                let stripes_fit_input_budget = stripes.iter().all(|s| {
                    let span = s.end_offset.saturating_sub(s.start_offset);
                    search_stripe_memory_bound(span, max_record_bytes) <= budget_cap
                });
                let result = if !encoding.is_utf16()
                    && tuning.requested_threads > 1
                    && stripes.len() > 1
                    && stripes_fit_input_budget
                {
                    run_parallel_search(
                        Arc::new(file),
                        file_size,
                        indexed_frontier,
                        indexed_rows,
                        delimiter,
                        &needle,
                        case_sensitivity,
                        start,
                        &stripes,
                        &cell_edits,
                        max_record_bytes,
                        encoding,
                        tuning,
                        &worker_state,
                    )
                } else {
                    run_search_bounded(
                        file,
                        file_size,
                        indexed_frontier,
                        indexed_rows,
                        delimiter,
                        &needle,
                        case_sensitivity,
                        start,
                        checkpoint,
                        &cell_edits,
                        max_record_bytes,
                        encoding,
                        &worker_state,
                    )
                };
                let result = match result {
                    Ok(outcome) => {
                        if let (Some(stamp), Some(guard_file)) = (&expected_source, &guard_file) {
                            match crate::export::source_matches_stamp(guard_file, &path, stamp) {
                                Ok(true) => Ok(outcome),
                                Ok(false) => Err(QuarryError::SourceChanged),
                                Err(error) => Err(error),
                            }
                        } else {
                            Ok(outcome)
                        }
                    }
                    Err(err @ QuarryError::RowNotIndexed { .. }) => {
                        if let (Some(stamp), Some(guard_file)) = (&expected_source, &guard_file) {
                            match crate::export::source_matches_stamp(guard_file, &path, stamp) {
                                Ok(true) => Err(err),
                                Ok(false) => Err(QuarryError::SourceChanged),
                                Err(error) => Err(error),
                            }
                        } else {
                            Err(err)
                        }
                    }
                    Err(error) => Err(error),
                };
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

    pub fn progress(&self) -> SearchProgress {
        let done = self.shared.done.load(Ordering::Acquire);
        let finished_nanos = self.shared.finished_nanos.load(Ordering::Acquire);
        SearchProgress {
            bytes_scanned: self.shared.bytes_scanned.load(Ordering::Acquire),
            rows_scanned: self.shared.rows_scanned.load(Ordering::Acquire),
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

    pub fn telemetry(&self) -> SearchTelemetry {
        *self.shared.telemetry.lock().unwrap()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
    }

    pub fn wait(mut self) -> Result<SearchOutcome, QuarryError> {
        self.handle
            .take()
            .expect("search handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl Drop for SearchJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

fn build_search_stripes(
    index: &StructuralIndex,
    start_cp: Checkpoint,
    indexed_frontier: u64,
    file_size: u64,
    stripe_target_bytes: u64,
) -> Vec<SearchStripe> {
    let mut stripes = Vec::new();
    let target = stripe_target_bytes.max(1);
    let mut cur = start_cp;
    let mut id = 0u64;

    for &cp in index.checkpoints() {
        if cp.offset <= cur.offset || cp.offset > indexed_frontier {
            continue;
        }
        if cp.offset - cur.offset >= target {
            stripes.push(SearchStripe {
                id,
                start_row: cur.row,
                start_offset: cur.offset,
                end_offset: cp.offset,
                is_eof: cp.offset == file_size,
            });
            id += 1;
            cur = cp;
        }
    }

    if cur.offset < indexed_frontier || (cur.offset == indexed_frontier && stripes.is_empty()) {
        stripes.push(SearchStripe {
            id,
            start_row: cur.row,
            start_offset: cur.offset,
            end_offset: indexed_frontier,
            is_eof: indexed_frontier == file_size,
        });
    }

    stripes
}

impl Session {
    pub fn start_search(
        &self,
        index: &StructuralIndex,
        needle: Vec<u8>,
        start: SearchPosition,
    ) -> Result<SearchJob, QuarryError> {
        self.start_search_with_options(
            index,
            needle,
            start,
            BTreeMap::new(),
            CaseSensitivity::Sensitive,
            None,
        )
    }

    pub fn start_search_with_case(
        &self,
        index: &StructuralIndex,
        needle: Vec<u8>,
        start: SearchPosition,
        case_sensitivity: CaseSensitivity,
    ) -> Result<SearchJob, QuarryError> {
        self.start_search_with_options(
            index,
            needle,
            start,
            BTreeMap::new(),
            case_sensitivity,
            None,
        )
    }

    pub fn start_search_with_cell_edits(
        &self,
        index: &StructuralIndex,
        needle: Vec<u8>,
        start: SearchPosition,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
    ) -> Result<SearchJob, QuarryError> {
        self.start_search_with_options(
            index,
            needle,
            start,
            cell_edits,
            CaseSensitivity::Sensitive,
            None,
        )
    }

    pub fn start_search_with_cell_edits_and_case(
        &self,
        index: &StructuralIndex,
        needle: Vec<u8>,
        start: SearchPosition,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        case_sensitivity: CaseSensitivity,
    ) -> Result<SearchJob, QuarryError> {
        self.start_search_with_options(index, needle, start, cell_edits, case_sensitivity, None)
    }

    pub fn start_search_with_options(
        &self,
        index: &StructuralIndex,
        needle: Vec<u8>,
        start: SearchPosition,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        case_sensitivity: CaseSensitivity,
        threads: Option<usize>,
    ) -> Result<SearchJob, QuarryError> {
        let requested_threads = resolve_worker_count(threads)?;
        self.start_search_with_tuning(
            index,
            needle,
            start,
            cell_edits,
            case_sensitivity,
            SearchTuning::for_production(requested_threads),
        )
    }

    fn start_search_with_tuning(
        &self,
        index: &StructuralIndex,
        needle: Vec<u8>,
        start: SearchPosition,
        mut cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        case_sensitivity: CaseSensitivity,
        tuning: SearchTuning,
    ) -> Result<SearchJob, QuarryError> {
        if needle.is_empty() {
            return Err(QuarryError::InvalidOption("search query must not be empty"));
        }
        let (section_base, section_end) = self.active_byte_range();
        if !index.matches_session(
            &self.source_stamp,
            self.dialect.delimiter,
            self.encoding,
            section_base,
            section_end,
            self.is_section(),
        ) {
            return Err(QuarryError::InvalidOption(
                "index does not match session source, framing, or byte range",
            ));
        }
        let data_start = u64::from(self.dialect.has_header && section_base == 0);
        cell_edits.retain(|(row, _), _| *row >= data_start);
        let start = if start.row < data_start {
            SearchPosition {
                row: data_start,
                column: 0,
            }
        } else {
            start
        };
        let indexed_frontier = if index.is_complete() {
            section_end
        } else {
            index.committed_record_end().min(section_end)
        };
        let checkpoint = index.nearest_checkpoint(start.row);
        let stripes = build_search_stripes(
            index,
            checkpoint,
            indexed_frontier,
            section_end,
            tuning.stripe_target_bytes,
        );
        SearchJob::start_plan(
            self.path.clone(),
            section_end,
            indexed_frontier,
            index.indexed_rows(),
            Some(self.source_stamp.clone()),
            self.dialect.delimiter,
            needle,
            case_sensitivity,
            start,
            checkpoint,
            stripes,
            cell_edits,
            DEFAULT_MAX_RECORD_BYTES,
            self.encoding,
            tuning,
        )
    }
}

struct StripeResult {
    id: u64,
    bytes: u64,
    rows: u64,
    outcome: Result<Option<SearchMatch>, QuarryError>,
}

#[allow(clippy::too_many_arguments)]
fn scan_search_stripe(
    stripe: SearchStripe,
    file: &File,
    delimiter: u8,
    needle: &[u8],
    case_sensitivity: CaseSensitivity,
    start: SearchPosition,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    edited_rows: &BTreeSet<u64>,
    max_record_bytes: usize,
    encoding: Encoding,
    stop_flag: &AtomicBool,
    earliest_hit_stripe: &AtomicU64,
    #[cfg(test)] wait_for_later_hit_before_stripe_zero: bool,
) -> Option<StripeResult> {
    #[cfg(test)]
    if stripe.id == 0 && wait_for_later_hit_before_stripe_zero {
        let deadline = Instant::now() + Duration::from_secs(5);
        while earliest_hit_stripe.load(Ordering::Acquire) == u64::MAX
            && !stop_flag.load(Ordering::Acquire)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
    }

    if stop_flag.load(Ordering::Acquire) || stripe.id > earliest_hit_stripe.load(Ordering::Acquire)
    {
        return None;
    }

    let span = stripe.end_offset.saturating_sub(stripe.start_offset);
    if span == 0 {
        return Some(StripeResult {
            id: stripe.id,
            bytes: 0,
            rows: 0,
            outcome: Ok(None),
        });
    }

    // Stream oversized stripes in bounded chunks so sparse checkpoints or multi-GB records
    // never allocate an unbounded buffer before enforcing `max_record_bytes`.
    if span > MAX_SINGLE_READ_STRIPE_BYTES || span > (max_record_bytes as u64).saturating_mul(2) {
        return scan_search_stripe_streaming(
            stripe,
            file,
            delimiter,
            needle,
            case_sensitivity,
            start,
            cell_edits,
            edited_rows,
            max_record_bytes,
            encoding,
            stop_flag,
            earliest_hit_stripe,
        );
    }

    let len = span as usize;

    let mut buf = vec![0u8; len];
    match read_at_exact_or_eof(file, stripe.start_offset, &mut buf) {
        Ok(read) if read == len => {}
        Ok(_) => {
            earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
            return Some(StripeResult {
                id: stripe.id,
                bytes: 0,
                rows: 0,
                outcome: Err(QuarryError::SourceChanged),
            });
        }
        Err(error) => {
            earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
            return Some(StripeResult {
                id: stripe.id,
                bytes: 0,
                rows: 0,
                outcome: Err(QuarryError::Io(error)),
            });
        }
    }

    let matcher = ByteMatcher::new(needle, case_sensitivity);
    let can_prefilter = encoding == Encoding::Utf8 && !needle.contains(&b'"');
    let raw_stripe_has_hit = !can_prefilter || matcher.find(&buf).is_some();

    let mut scanner = match RecordScanner::at_offset(delimiter, stripe.start_offset, encoding) {
        Ok(scanner) => scanner,
        Err(error) => {
            earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
            return Some(StripeResult {
                id: stripe.id,
                bytes: 0,
                rows: 0,
                outcome: Err(error),
            });
        }
    };

    let mut row_number = stripe.start_row;
    let mut record_start = stripe.start_offset;
    let mut rows_scanned = 0u64;
    let mut found: Option<SearchMatch> = None;
    let mut deferred_error: Option<QuarryError> = None;

    let mut offset = 0usize;
    while offset < buf.len() {
        if stop_flag.load(Ordering::Acquire)
            || stripe.id > earliest_hit_stripe.load(Ordering::Acquire)
        {
            return None;
        }
        let slice_end = (offset + WORKER_CANCEL_SLICE_BYTES).min(buf.len());
        let slice_abs = stripe.start_offset + offset as u64;
        let scan_res = scanner.scan_chunk(&buf[offset..slice_end], slice_abs, |absolute_end| {
            if found.is_none() && deferred_error.is_none() {
                let rec_len = (absolute_end - record_start) as usize;
                if row_number >= start.row {
                    if rec_len > max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    } else {
                        let rec_local_start = (record_start - stripe.start_offset) as usize;
                        let rec_local_end = (absolute_end - stripe.start_offset) as usize;
                        let rec_bytes = &buf[rec_local_start..rec_local_end];
                        let row_has_edit =
                            !edited_rows.is_empty() && edited_rows.contains(&row_number);
                        let should_parse = row_has_edit
                            || (raw_stripe_has_hit
                                && (!can_prefilter || matcher.find(rec_bytes).is_some()));
                        if should_parse {
                            match find_record(
                                &matcher,
                                rec_bytes,
                                delimiter,
                                row_number,
                                record_start,
                                start,
                                cell_edits,
                                encoding,
                            ) {
                                Ok(result) => found = result,
                                Err(error) => deferred_error = Some(error),
                            }
                        }
                    }
                }
            }
            row_number += 1;
            record_start = absolute_end;
            rows_scanned += 1;
        });

        if let Some(error) = deferred_error {
            earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
            return Some(StripeResult {
                id: stripe.id,
                bytes: (slice_end as u64),
                rows: rows_scanned,
                outcome: Err(error),
            });
        }
        if let Some(hit) = found {
            earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
            return Some(StripeResult {
                id: stripe.id,
                bytes: (slice_end as u64),
                rows: rows_scanned,
                outcome: Ok(Some(hit)),
            });
        }
        if let Err(error) = scan_res {
            earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
            return Some(StripeResult {
                id: stripe.id,
                bytes: (slice_end as u64),
                rows: rows_scanned,
                outcome: Err(error),
            });
        }

        offset = slice_end;
    }

    if stripe.is_eof {
        match scanner.finish(stripe.end_offset, |_| {}) {
            Ok(true) => {
                rows_scanned += 1;
                if row_number >= start.row {
                    let rec_local_start = (record_start - stripe.start_offset) as usize;
                    let rec_bytes = &buf[rec_local_start..];
                    if rec_bytes.len() > max_record_bytes {
                        earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                        return Some(StripeResult {
                            id: stripe.id,
                            bytes: len as u64,
                            rows: rows_scanned,
                            outcome: Err(QuarryError::RecordTooLarge {
                                limit: max_record_bytes,
                            }),
                        });
                    }
                    let row_has_edit = !edited_rows.is_empty() && edited_rows.contains(&row_number);
                    let should_parse = row_has_edit
                        || (raw_stripe_has_hit
                            && (!can_prefilter || matcher.find(rec_bytes).is_some()));
                    if should_parse {
                        match find_record(
                            &matcher,
                            rec_bytes,
                            delimiter,
                            row_number,
                            record_start,
                            start,
                            cell_edits,
                            encoding,
                        ) {
                            Ok(Some(hit)) => {
                                earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                                return Some(StripeResult {
                                    id: stripe.id,
                                    bytes: len as u64,
                                    rows: rows_scanned,
                                    outcome: Ok(Some(hit)),
                                });
                            }
                            Ok(None) => {}
                            Err(error) => {
                                earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                                return Some(StripeResult {
                                    id: stripe.id,
                                    bytes: len as u64,
                                    rows: rows_scanned,
                                    outcome: Err(error),
                                });
                            }
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(error) => {
                earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                return Some(StripeResult {
                    id: stripe.id,
                    bytes: len as u64,
                    rows: rows_scanned,
                    outcome: Err(error),
                });
            }
        }
    }

    Some(StripeResult {
        id: stripe.id,
        bytes: len as u64,
        rows: rows_scanned,
        outcome: Ok(None),
    })
}

/// Appends `bytes` to `record` while keeping `record.capacity() <= max_cap` (never doubling past
/// `max_cap`, so transient reallocation holds at most `2 * max_cap`).
fn append_bounded_record(record: &mut Vec<u8>, bytes: &[u8], max_cap: usize) {
    let needed = record.len().saturating_add(bytes.len());
    if needed > record.capacity() {
        let next_cap = record.capacity().saturating_mul(2).max(needed).min(max_cap);
        record.reserve_exact(next_cap.saturating_sub(record.len()));
    }
    record.extend_from_slice(bytes);
}

/// Bounded streaming stripe scan for stripes whose span exceeds `MAX_SINGLE_READ_STRIPE_BYTES`
/// or `2 * max_record_bytes`, enforcing `max_record_bytes` incrementally without buffering the
/// entire stripe in memory.
#[allow(clippy::too_many_arguments)]
fn scan_search_stripe_streaming(
    stripe: SearchStripe,
    file: &File,
    delimiter: u8,
    needle: &[u8],
    case_sensitivity: CaseSensitivity,
    start: SearchPosition,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    edited_rows: &BTreeSet<u64>,
    max_record_bytes: usize,
    encoding: Encoding,
    stop_flag: &AtomicBool,
    earliest_hit_stripe: &AtomicU64,
) -> Option<StripeResult> {
    let matcher = ByteMatcher::new(needle, case_sensitivity);
    let can_prefilter = encoding == Encoding::Utf8 && !needle.contains(&b'"');
    let mut scanner = match RecordScanner::at_offset(delimiter, stripe.start_offset, encoding) {
        Ok(scanner) => scanner,
        Err(error) => {
            earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
            return Some(StripeResult {
                id: stripe.id,
                bytes: 0,
                rows: 0,
                outcome: Err(error),
            });
        }
    };

    let chunk_cap = encoding
        .chunk_size(WORKER_CANCEL_SLICE_BYTES.min(DEFAULT_READ_CHUNK))
        .max(2);
    let mut chunk = vec![0u8; chunk_cap];
    let mut absolute_start = stripe.start_offset;
    let mut row_number = stripe.start_row;
    let mut record_start = stripe.start_offset;
    let mut rows_scanned = 0u64;
    let mut record = Vec::new();

    while absolute_start < stripe.end_offset {
        if stop_flag.load(Ordering::Acquire)
            || stripe.id > earliest_hit_stripe.load(Ordering::Acquire)
        {
            return None;
        }
        let remaining = stripe.end_offset - absolute_start;
        let mut want = remaining.min(chunk.len() as u64) as usize;
        if matches!(encoding, Encoding::Utf16Le | Encoding::Utf16Be) && want > 1 {
            want &= !1;
        }
        let read = match read_at_exact_or_eof(file, absolute_start, &mut chunk[..want]) {
            Ok(n) if n == want => n,
            Ok(_) => {
                earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                return Some(StripeResult {
                    id: stripe.id,
                    bytes: absolute_start - stripe.start_offset,
                    rows: rows_scanned,
                    outcome: Err(QuarryError::SourceChanged),
                });
            }
            Err(error) => {
                earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                return Some(StripeResult {
                    id: stripe.id,
                    bytes: absolute_start - stripe.start_offset,
                    rows: rows_scanned,
                    outcome: Err(QuarryError::Io(error)),
                });
            }
        };

        let chunk_has_raw_hit = !can_prefilter || matcher.find(&chunk[..read]).is_some();
        let mut segment_start = 0usize;
        let mut found: Option<SearchMatch> = None;
        let mut deferred_error: Option<QuarryError> = None;
        let scan_res = scanner.scan_chunk(&chunk[..read], absolute_start, |absolute_end| {
            let local_end = (absolute_end - absolute_start) as usize;
            if found.is_none() && deferred_error.is_none() && row_number >= start.row {
                let row_has_edit = !edited_rows.is_empty() && edited_rows.contains(&row_number);
                if record.is_empty() {
                    let rec_bytes = &chunk[segment_start..local_end];
                    if rec_bytes.len() > max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    } else if row_has_edit
                        || (chunk_has_raw_hit
                            && (!can_prefilter || matcher.find(rec_bytes).is_some()))
                    {
                        match find_record(
                            &matcher,
                            rec_bytes,
                            delimiter,
                            row_number,
                            record_start,
                            start,
                            cell_edits,
                            encoding,
                        ) {
                            Ok(res) => found = res,
                            Err(err) => deferred_error = Some(err),
                        }
                    }
                } else {
                    let total_len = record.len().saturating_add(local_end - segment_start);
                    if total_len > max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    } else {
                        let record_cap = (stripe.end_offset - stripe.start_offset)
                            .min(max_record_bytes as u64)
                            as usize;
                        append_bounded_record(
                            &mut record,
                            &chunk[segment_start..local_end],
                            record_cap,
                        );
                        if row_has_edit || !can_prefilter || matcher.find(&record).is_some() {
                            match find_record(
                                &matcher,
                                &record,
                                delimiter,
                                row_number,
                                record_start,
                                start,
                                cell_edits,
                                encoding,
                            ) {
                                Ok(res) => found = res,
                                Err(err) => deferred_error = Some(err),
                            }
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
        if let Some(error) = deferred_error {
            earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
            return Some(StripeResult {
                id: stripe.id,
                bytes: absolute_start - stripe.start_offset,
                rows: rows_scanned,
                outcome: Err(error),
            });
        }
        if let Some(hit) = found {
            earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
            return Some(StripeResult {
                id: stripe.id,
                bytes: absolute_start - stripe.start_offset,
                rows: rows_scanned,
                outcome: Ok(Some(hit)),
            });
        }
        if let Err(error) = scan_res {
            earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
            return Some(StripeResult {
                id: stripe.id,
                bytes: absolute_start - stripe.start_offset,
                rows: rows_scanned,
                outcome: Err(error),
            });
        }

        if row_number >= start.row {
            let tail_len = read - segment_start;
            if record.len().saturating_add(tail_len) > max_record_bytes {
                earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                return Some(StripeResult {
                    id: stripe.id,
                    bytes: absolute_start - stripe.start_offset,
                    rows: rows_scanned,
                    outcome: Err(QuarryError::RecordTooLarge {
                        limit: max_record_bytes,
                    }),
                });
            }
            let record_cap =
                (stripe.end_offset - stripe.start_offset).min(max_record_bytes as u64) as usize;
            append_bounded_record(&mut record, &chunk[segment_start..read], record_cap);
        }
    }

    if stripe.is_eof {
        match scanner.finish(stripe.end_offset, |_| {}) {
            Ok(true) => {
                rows_scanned += 1;
                if row_number >= start.row {
                    if record.len() > max_record_bytes {
                        earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                        return Some(StripeResult {
                            id: stripe.id,
                            bytes: stripe.end_offset - stripe.start_offset,
                            rows: rows_scanned,
                            outcome: Err(QuarryError::RecordTooLarge {
                                limit: max_record_bytes,
                            }),
                        });
                    }
                    let row_has_edit = !edited_rows.is_empty() && edited_rows.contains(&row_number);
                    if row_has_edit || !can_prefilter || matcher.find(&record).is_some() {
                        match find_record(
                            &matcher,
                            &record,
                            delimiter,
                            row_number,
                            record_start,
                            start,
                            cell_edits,
                            encoding,
                        ) {
                            Ok(Some(hit)) => {
                                earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                                return Some(StripeResult {
                                    id: stripe.id,
                                    bytes: stripe.end_offset - stripe.start_offset,
                                    rows: rows_scanned,
                                    outcome: Ok(Some(hit)),
                                });
                            }
                            Ok(None) => {}
                            Err(error) => {
                                earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                                return Some(StripeResult {
                                    id: stripe.id,
                                    bytes: stripe.end_offset - stripe.start_offset,
                                    rows: rows_scanned,
                                    outcome: Err(error),
                                });
                            }
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(error) => {
                earliest_hit_stripe.fetch_min(stripe.id, Ordering::AcqRel);
                return Some(StripeResult {
                    id: stripe.id,
                    bytes: stripe.end_offset - stripe.start_offset,
                    rows: rows_scanned,
                    outcome: Err(error),
                });
            }
        }
    }

    Some(StripeResult {
        id: stripe.id,
        bytes: stripe.end_offset - stripe.start_offset,
        rows: rows_scanned,
        outcome: Ok(None),
    })
}

/// Conservative peak live memory bound for scanning a search stripe.
///
/// Single-read stripes buffer the entire stripe (`<= MAX_SINGLE_READ_STRIPE_BYTES`).
/// Streaming stripes hold a `WORKER_CANCEL_SLICE_BYTES` read buffer plus one active
/// record buffer whose capacity is capped at `stripe_span.min(max_record_bytes)` via
/// `append_bounded_record` (and transiently up to `2 * record_cap` while reallocating).
fn search_stripe_memory_bound(stripe_span: u64, max_record_bytes: usize) -> u64 {
    if stripe_span > MAX_SINGLE_READ_STRIPE_BYTES
        || stripe_span > (max_record_bytes as u64).saturating_mul(2)
    {
        let record_cap_bound = stripe_span.min(max_record_bytes as u64);
        record_cap_bound
            .saturating_mul(2)
            .saturating_add(WORKER_CANCEL_SLICE_BYTES as u64)
            .max(1)
    } else {
        stripe_span.max(1)
    }
}

#[allow(clippy::too_many_arguments)]
fn run_parallel_search(
    file: Arc<File>,
    file_size: u64,
    indexed_frontier: u64,
    indexed_rows: u64,
    delimiter: u8,
    needle: &[u8],
    case_sensitivity: CaseSensitivity,
    start: SearchPosition,
    stripes: &[SearchStripe],
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    max_record_bytes: usize,
    encoding: Encoding,
    tuning: SearchTuning,
    shared: &SharedState,
) -> Result<SearchOutcome, QuarryError> {
    let effective_workers = tuning.requested_threads.min(stripes.len()).max(1);
    let pool = engine_pool();
    let executor = JobExecutor::new(pool, JobPriority::Foreground, effective_workers)?;
    let effective_workers = executor.effective_workers();
    let max_stripe_bytes = stripes
        .iter()
        .map(|s| s.end_offset.saturating_sub(s.start_offset))
        .max()
        .unwrap_or(0);
    let window = effective_workers.clamp(1, 16);
    {
        let mut t = shared.telemetry.lock().unwrap();
        t.effective_workers = effective_workers;
        t.wasted_work_bound_bytes = (window as u64).saturating_mul(max_stripe_bytes);
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let earliest_hit_stripe = Arc::new(AtomicU64::new(u64::MAX));
    let (tx, rx) = mpsc::channel::<StripeResult>();

    let needle_arc = Arc::new(needle.to_vec());
    let edits_arc = Arc::new(cell_edits.clone());
    let edited_rows_arc: Arc<BTreeSet<u64>> =
        Arc::new(cell_edits.keys().map(|(row, _)| *row).collect());

    let mut pending: BTreeMap<u64, StripeResult> = BTreeMap::new();
    let mut next_merge_id = 0u64;
    let mut next_dispatch_idx = 0usize;
    let mut bytes_done = 0u64;
    let mut rows_done = 0u64;
    let mut stripes_done = 0u64;

    let mut final_outcome: Option<Result<SearchOutcome, QuarryError>> = None;

    while final_outcome.is_none() {
        if shared.cancelled.load(Ordering::Acquire) {
            stop_flag.store(true, Ordering::Release);
            final_outcome = Some(Ok(SearchOutcome::Cancelled));
            break;
        }

        // Dispatch up to `window` in-flight stripes, never dispatching past `earliest_hit_stripe`.
        while next_dispatch_idx < stripes.len()
            && (next_dispatch_idx as u64 - next_merge_id) < window as u64
            && (next_dispatch_idx as u64) < earliest_hit_stripe.load(Ordering::Acquire)
        {
            if shared.cancelled.load(Ordering::Acquire) {
                stop_flag.store(true, Ordering::Release);
                final_outcome = Some(Ok(SearchOutcome::Cancelled));
                break;
            }
            let stripe = stripes[next_dispatch_idx];
            next_dispatch_idx += 1;
            let stripe_span = stripe.end_offset.saturating_sub(stripe.start_offset);
            let budget_request = search_stripe_memory_bound(stripe_span, max_record_bytes) as usize;
            let Some(budget_guard) = pool
                .byte_budget()
                .acquire(budget_request, &shared.cancelled)
            else {
                stop_flag.store(true, Ordering::Release);
                final_outcome = Some(Ok(SearchOutcome::Cancelled));
                break;
            };
            advise_readahead(
                &file,
                stripe.start_offset,
                stripe_span.min(MAX_SINGLE_READ_STRIPE_BYTES),
            );

            let worker_file = Arc::clone(&file);
            let worker_needle = Arc::clone(&needle_arc);
            let worker_edits = Arc::clone(&edits_arc);
            let worker_edited_rows = Arc::clone(&edited_rows_arc);
            let worker_stop = Arc::clone(&stop_flag);
            let worker_earliest = Arc::clone(&earliest_hit_stripe);
            let worker_tx = tx.clone();
            #[cfg(test)]
            let wait_zero = tuning.wait_for_later_hit_before_stripe_zero;

            if !executor.spawn(&shared.cancelled, move || {
                let _budget = budget_guard;
                let res = scan_search_stripe(
                    stripe,
                    &worker_file,
                    delimiter,
                    &worker_needle,
                    case_sensitivity,
                    start,
                    &worker_edits,
                    &worker_edited_rows,
                    max_record_bytes,
                    encoding,
                    &worker_stop,
                    &worker_earliest,
                    #[cfg(test)]
                    wait_zero,
                );
                drop(_budget);
                if let Some(res) = res {
                    let _ = worker_tx.send(res);
                }
            }) {
                stop_flag.store(true, Ordering::Release);
                final_outcome = Some(Ok(SearchOutcome::Cancelled));
                break;
            }
        }

        if final_outcome.is_some() {
            break;
        }

        // Check if all stripes have been merged cleanly.
        if next_merge_id == stripes.len() as u64 {
            final_outcome = Some(if indexed_frontier == file_size {
                Ok(SearchOutcome::NotFound)
            } else {
                Err(QuarryError::RowNotIndexed {
                    requested: indexed_rows,
                    indexed_rows,
                })
            });
            break;
        }

        // Receive completed stripes and merge in strict stripe order `0, 1, 2, ...`.
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(result) => {
                pending.insert(result.id, result);
                while let Ok(more) = rx.try_recv() {
                    pending.insert(more.id, more);
                }
                while let Some(res) = pending.remove(&next_merge_id) {
                    next_merge_id += 1;
                    stripes_done += 1;
                    bytes_done = bytes_done.saturating_add(res.bytes);
                    rows_done = rows_done.saturating_add(res.rows);
                    shared.bytes_scanned.store(bytes_done, Ordering::Release);
                    shared.rows_scanned.store(rows_done, Ordering::Release);
                    match res.outcome {
                        Ok(Some(hit)) => {
                            stop_flag.store(true, Ordering::Release);
                            final_outcome = Some(Ok(SearchOutcome::Match(hit)));
                            break;
                        }
                        Err(error) => {
                            stop_flag.store(true, Ordering::Release);
                            final_outcome = Some(Err(error));
                            break;
                        }
                        Ok(None) => {}
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if executor.has_panicked() {
                    stop_flag.store(true, Ordering::Release);
                    final_outcome = Some(Err(QuarryError::WorkerPanicked));
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                final_outcome = Some(Ok(SearchOutcome::Cancelled));
                break;
            }
        }
    }

    stop_flag.store(true, Ordering::Release);
    drop(tx);
    let drain_res = executor.drain();
    shared.telemetry.lock().unwrap().stripes_scanned = stripes_done;
    let outcome = final_outcome.expect("search loop terminates with outcome");
    drain_res?;
    outcome
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn run_search(
    file: File,
    delimiter: u8,
    needle: &[u8],
    case_sensitivity: CaseSensitivity,
    start: SearchPosition,
    checkpoint: Checkpoint,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    max_record_bytes: usize,
    encoding: Encoding,
    shared: &SharedState,
) -> Result<SearchOutcome, QuarryError> {
    let file_size = file.metadata()?.len();
    run_search_bounded(
        file,
        file_size,
        file_size,
        0,
        delimiter,
        needle,
        case_sensitivity,
        start,
        checkpoint,
        cell_edits,
        max_record_bytes,
        encoding,
        shared,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_search_bounded(
    mut file: File,
    file_size: u64,
    indexed_frontier: u64,
    indexed_rows: u64,
    delimiter: u8,
    needle: &[u8],
    case_sensitivity: CaseSensitivity,
    start: SearchPosition,
    checkpoint: Checkpoint,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    max_record_bytes: usize,
    encoding: Encoding,
    shared: &SharedState,
) -> Result<SearchOutcome, QuarryError> {
    let matcher = ByteMatcher::new(needle, case_sensitivity);
    let can_prefilter = encoding == Encoding::Utf8 && !needle.contains(&b'"');
    let edited_rows: BTreeSet<u64> = cell_edits.keys().map(|(row, _)| *row).collect();
    let mut scanner = RecordScanner::at_offset(delimiter, checkpoint.offset, encoding)?;
    let mut chunk = vec![0; encoding.chunk_size(DEFAULT_READ_CHUNK)];
    let mut absolute_start = checkpoint.offset;
    let mut row_number = checkpoint.row;
    let mut record_start = checkpoint.offset;
    let mut records_scanned = 0_u64;
    let mut record = Vec::new();

    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(SearchOutcome::Cancelled);
        }
        let remaining = indexed_frontier.saturating_sub(absolute_start);
        let read = if remaining == 0 {
            0
        } else {
            let cap = encoding.chunk_size((remaining as usize).min(chunk.len()));
            encoding.read_chunk(&mut file, &mut chunk[..cap])?
        };
        if read == 0 {
            if remaining > 0 {
                return Err(QuarryError::SourceChanged);
            }
            if indexed_frontier == file_size {
                let has_final_record = scanner.finish(absolute_start, |_| {})?;
                if has_final_record {
                    records_scanned += 1;
                    shared
                        .rows_scanned
                        .store(records_scanned, Ordering::Release);
                    if row_number >= start.row {
                        if record.len() > max_record_bytes {
                            return Err(QuarryError::RecordTooLarge {
                                limit: max_record_bytes,
                            });
                        }
                        if let Some(found) = find_record(
                            &matcher,
                            &record,
                            delimiter,
                            row_number,
                            record_start,
                            start,
                            cell_edits,
                            encoding,
                        )? {
                            return Ok(SearchOutcome::Match(found));
                        }
                    }
                }
                shared
                    .bytes_scanned
                    .store(absolute_start - checkpoint.offset, Ordering::Release);
                return Ok(SearchOutcome::NotFound);
            }
            shared
                .bytes_scanned
                .store(absolute_start - checkpoint.offset, Ordering::Release);
            return Err(QuarryError::RowNotIndexed {
                requested: indexed_rows,
                indexed_rows,
            });
        }

        let chunk_has_raw_hit = !can_prefilter || matcher.find(&chunk[..read]).is_some();
        let mut segment_start = 0;
        let mut found = None;
        let mut deferred_error = None;
        let mut cancelled = false;
        let scan_result = scanner.scan_chunk(&chunk[..read], absolute_start, |absolute_end| {
            let local_end = (absolute_end - absolute_start) as usize;
            if found.is_none() && deferred_error.is_none() && !cancelled {
                if shared.cancelled.load(Ordering::Acquire) {
                    cancelled = true;
                } else if row_number >= start.row {
                    let row_has_edit = !edited_rows.is_empty() && edited_rows.contains(&row_number);
                    if record.is_empty() {
                        let rec_bytes = &chunk[segment_start..local_end];
                        if rec_bytes.len() > max_record_bytes {
                            deferred_error = Some(QuarryError::RecordTooLarge {
                                limit: max_record_bytes,
                            });
                        } else if row_has_edit
                            || (chunk_has_raw_hit
                                && (!can_prefilter || matcher.find(rec_bytes).is_some()))
                        {
                            match find_record(
                                &matcher,
                                rec_bytes,
                                delimiter,
                                row_number,
                                record_start,
                                start,
                                cell_edits,
                                encoding,
                            ) {
                                Ok(result) => found = result,
                                Err(error) => deferred_error = Some(error),
                            }
                        }
                    } else {
                        record.extend_from_slice(&chunk[segment_start..local_end]);
                        if record.len() > max_record_bytes {
                            deferred_error = Some(QuarryError::RecordTooLarge {
                                limit: max_record_bytes,
                            });
                        } else if row_has_edit || !can_prefilter || matcher.find(&record).is_some()
                        {
                            match find_record(
                                &matcher,
                                &record,
                                delimiter,
                                row_number,
                                record_start,
                                start,
                                cell_edits,
                                encoding,
                            ) {
                                Ok(result) => found = result,
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
        shared
            .bytes_scanned
            .store(absolute_start - checkpoint.offset, Ordering::Release);
        shared
            .rows_scanned
            .store(records_scanned, Ordering::Release);
        if let Some(found) = found {
            return Ok(SearchOutcome::Match(found));
        }
        if cancelled || shared.cancelled.load(Ordering::Acquire) {
            return Ok(SearchOutcome::Cancelled);
        }
        if let Some(error) = deferred_error {
            return Err(error);
        }
        scan_result?;

        if row_number >= start.row {
            record.extend_from_slice(&chunk[segment_start..read]);
            if record.len() > max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: max_record_bytes,
                });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn find_record(
    matcher: &ByteMatcher<'_>,
    record: &[u8],
    delimiter: u8,
    row: u64,
    record_offset: u64,
    start: SearchPosition,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    encoding: Encoding,
) -> Result<Option<SearchMatch>, QuarryError> {
    let first_column = if row == start.row { start.column } else { 0 };
    let fields =
        crate::parse_encoded_record_with_bom(record, delimiter, row, record_offset == 0, encoding)?;
    Ok(fields
        .iter()
        .enumerate()
        .skip(first_column)
        .find(|(column, field)| {
            let value = cell_edits
                .get(&(row, *column))
                .map_or_else(|| field.as_ref(), Vec::as_slice);
            matcher.find(value).is_some()
        })
        .map(|(column, _)| SearchMatch {
            row,
            column,
            record_offset,
        }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::{
        SearchJob, SearchOutcome, SearchPosition, SearchTuning, SharedState, WorkerCompletion,
        run_search,
    };
    use crate::{
        CaseSensitivity, Checkpoint, DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, HeaderMode,
        IndexConfig, OpenOptions, QuarryError, Session,
    };

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture(bytes: &[u8]) -> std::path::PathBuf {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("quarry-search-{}-{id}.csv", std::process::id()));
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    fn session_and_index(
        path: &std::path::Path,
        has_header: bool,
    ) -> (Session, crate::StructuralIndex) {
        let session = Session::open(
            path,
            OpenOptions {
                header_mode: if has_header {
                    HeaderMode::FirstRow
                } else {
                    HeaderMode::NoHeader
                },
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let index = session
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        (session, index)
    }

    #[test]
    fn finds_decoded_quoted_multiline_cell_across_chunks() {
        let mut bytes = b"name,value\n\"".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', DEFAULT_READ_CHUNK));
        bytes.extend_from_slice(b"\ntarget \"\"quoted\"\"\",tail\n");
        let expected_offset = b"name,value\n".len() as u64;
        let path = fixture(&bytes);
        let (session, index) = session_and_index(&path, true);

        let outcome = session
            .start_search(
                &index,
                b"\ntarget \"quoted\"".to_vec(),
                SearchPosition { row: 1, column: 0 },
            )
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(
            outcome,
            SearchOutcome::Match(super::SearchMatch {
                row: 1,
                column: 0,
                record_offset: expected_offset,
            })
        );
        assert_eq!(
            session
                .start_search(
                    &index,
                    b"\"\"quoted\"\"".to_vec(),
                    SearchPosition { row: 1, column: 0 },
                )
                .unwrap()
                .wait()
                .unwrap(),
            SearchOutcome::NotFound
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_case_mode_honors_cursor_and_completes_exactly() {
        let path = fixture(b"needle,header\nNeedle,needle\nneedle,tail\n");
        let (session, index) = session_and_index(&path, true);

        let insensitive = session
            .start_search_with_case(
                &index,
                b"needle".to_vec(),
                SearchPosition { row: 0, column: 0 },
                CaseSensitivity::Insensitive,
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(
            insensitive,
            SearchOutcome::Match(super::SearchMatch {
                row: 1,
                column: 0,
                ..
            })
        ));

        let first = session
            .start_search(
                &index,
                b"needle".to_vec(),
                SearchPosition { row: 0, column: 1 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(
            first,
            SearchOutcome::Match(super::SearchMatch {
                row: 1,
                column: 1,
                ..
            })
        ));

        let second = session
            .start_search(
                &index,
                b"needle".to_vec(),
                SearchPosition { row: 1, column: 2 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(
            second,
            SearchOutcome::Match(super::SearchMatch {
                row: 2,
                column: 0,
                ..
            })
        ));

        let job = session
            .start_search(
                &index,
                b"missing".to_vec(),
                SearchPosition { row: 2, column: 1 },
            )
            .unwrap();
        while !job.progress().done {
            thread::yield_now();
        }
        let progress = job.progress();
        assert_eq!(progress.bytes_scanned, progress.total_bytes);
        assert!(progress.rows_scanned > 0);
        assert!(progress.elapsed > Duration::ZERO);
        assert!(!progress.cancelled);
        assert_eq!(job.wait().unwrap(), SearchOutcome::NotFound);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_uses_sparse_cell_edits_as_effective_values() {
        let path = fixture(b"left,right\nsource-hit,plain\nother,source-only\n");
        let (session, index) = session_and_index(&path, true);
        let edits = BTreeMap::from([
            ((1, 0), b"edited".to_vec()),
            ((2, 1), b"overlay-hit".to_vec()),
        ]);

        assert_eq!(
            session
                .start_search_with_cell_edits(
                    &index,
                    b"source-hit".to_vec(),
                    SearchPosition { row: 0, column: 0 },
                    edits.clone(),
                )
                .unwrap()
                .wait()
                .unwrap(),
            SearchOutcome::NotFound
        );
        assert!(matches!(
            session
                .start_search_with_cell_edits(
                    &index,
                    b"overlay-hit".to_vec(),
                    SearchPosition { row: 0, column: 0 },
                    edits,
                )
                .unwrap()
                .wait()
                .unwrap(),
            SearchOutcome::Match(super::SearchMatch {
                row: 2,
                column: 1,
                ..
            })
        ));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_ignores_cell_edits_without_existing_data_cells() {
        let path = fixture(b"header\nvalue\n");
        let (session, index) = session_and_index(&path, true);
        let edits = BTreeMap::from([
            ((0, 0), b"impossible-hit".to_vec()),
            ((1, 9), b"impossible-hit".to_vec()),
            ((99, 0), b"impossible-hit".to_vec()),
        ]);

        assert_eq!(
            session
                .start_search_with_cell_edits(
                    &index,
                    b"impossible-hit".to_vec(),
                    SearchPosition { row: 0, column: 0 },
                    edits,
                )
                .unwrap()
                .wait()
                .unwrap(),
            SearchOutcome::NotFound
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn finds_literal_matches_inside_quoted_and_multiline_fields_only() {
        let path = fixture(
            b"id,note\n1,\"alpha,beta\"\n2,\"he said \"\"target\"\"\nnow\"\n3,target_end\n",
        );
        let (session, index) = session_and_index(&path, true);

        let comma = session
            .start_search(&index, b",".to_vec(), SearchPosition { row: 1, column: 0 })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            comma,
            SearchOutcome::Match(super::SearchMatch {
                row: 1,
                column: 1,
                record_offset: 8,
            })
        );

        let quoted = session
            .start_search(
                &index,
                b"\"target\"\nnow".to_vec(),
                SearchPosition { row: 1, column: 0 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            quoted,
            SearchOutcome::Match(super::SearchMatch {
                row: 2,
                column: 1,
                record_offset: 23,
            })
        );

        let insensitive = session
            .start_search_with_case(
                &index,
                b"TARGET".to_vec(),
                SearchPosition { row: 1, column: 0 },
                CaseSensitivity::Insensitive,
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            insensitive,
            SearchOutcome::Match(super::SearchMatch {
                row: 2,
                column: 1,
                record_offset: 23,
            })
        );

        let next = session
            .start_search(
                &index,
                b"target".to_vec(),
                SearchPosition { row: 2, column: 2 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            next,
            SearchOutcome::Match(super::SearchMatch {
                row: 3,
                column: 1,
                record_offset: 50,
            })
        );

        let header = session
            .start_search(
                &index,
                b"note".to_vec(),
                SearchPosition { row: 0, column: 0 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(header, SearchOutcome::NotFound);

        let missing = session
            .start_search(
                &index,
                b"absent".to_vec(),
                SearchPosition { row: 1, column: 0 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(missing, SearchOutcome::NotFound);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_respects_sparse_cell_edits_without_matching_replaced_text() {
        let path = fixture(b"id,note\n1,old_value\n2,plain\n");
        let (session, index) = session_and_index(&path, true);
        let edits = BTreeMap::from([
            ((0, 1), b"header_ignored".to_vec()),
            ((1, 1), b"new_value".to_vec()),
            ((2, 1), b"patched".to_vec()),
        ]);

        let replaced = session
            .start_search_with_cell_edits(
                &index,
                b"old_value".to_vec(),
                SearchPosition { row: 1, column: 0 },
                edits.clone(),
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(replaced, SearchOutcome::NotFound);

        let inserted = session
            .start_search_with_cell_edits(
                &index,
                b"new_value".to_vec(),
                SearchPosition { row: 1, column: 0 },
                edits.clone(),
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            inserted,
            SearchOutcome::Match(super::SearchMatch {
                row: 1,
                column: 1,
                record_offset: 8,
            })
        );

        let later = session
            .start_search_with_cell_edits(
                &index,
                b"patched".to_vec(),
                SearchPosition { row: 1, column: 0 },
                edits,
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            later,
            SearchOutcome::Match(super::SearchMatch {
                row: 2,
                column: 1,
                record_offset: 20,
            })
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn searches_unterminated_final_record_from_a_checkpoint() {
        let path = fixture(b"a,b\r\nc,target");
        let session = Session::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let index = session
            .start_indexing(IndexConfig {
                chunk_bytes: 2,
                checkpoint_every: 1,
                memory_budget_bytes: 64,
            })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            index.nearest_checkpoint(1),
            Checkpoint { row: 1, offset: 5 }
        );

        let outcome = session
            .start_search(
                &index,
                b"target".to_vec(),
                SearchPosition { row: 1, column: 0 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            outcome,
            SearchOutcome::Match(super::SearchMatch {
                row: 1,
                column: 1,
                record_offset: 5,
            })
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn parallel_search_slow_first_stripe_beats_faster_competing_later_hit() {
        let mut data = Vec::new();
        data.extend_from_slice(b"id,text\n");
        // With checkpoint_every = 20 and stripe_target_bytes = 256:
        // stripe 0 covers rows 0..20 (hit at row 5), stripe 1 covers rows 20..40 (hit at row 30).
        for i in 1..=100_u32 {
            if i == 5 || i == 30 {
                data.extend_from_slice(format!("{i},needle_hit_{i}\n").as_bytes());
            } else {
                data.extend_from_slice(format!("{i},ordinary_row_{i:04}\n").as_bytes());
            }
        }
        let path = fixture(&data);
        let session = Session::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let index = session
            .start_indexing(IndexConfig {
                chunk_bytes: 64,
                checkpoint_every: 20,
                memory_budget_bytes: 64 * 1024,
            })
            .unwrap()
            .wait()
            .unwrap();

        let job = session
            .start_search_with_tuning(
                &index,
                b"needle_hit".to_vec(),
                SearchPosition { row: 1, column: 0 },
                BTreeMap::new(),
                CaseSensitivity::Sensitive,
                SearchTuning {
                    requested_threads: 4,
                    stripe_target_bytes: 256,
                    wait_for_later_hit_before_stripe_zero: true,
                },
            )
            .unwrap();
        let outcome = job.wait().unwrap();
        match outcome {
            SearchOutcome::Match(found) => assert_eq!(found.row, 5),
            other => panic!("expected earliest hit on row 5, got {other:?}"),
        }

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn parallel_search_preceding_parse_error_beats_later_stripe_hit() {
        let mut data = Vec::new();
        data.extend_from_slice(b"id,text\n");
        for i in 1..=100_u32 {
            if i == 5 {
                data.extend_from_slice(b"5,\"bad\"trailing\n");
            } else if i == 30 {
                data.extend_from_slice(b"30,needle_hit\n");
            } else {
                data.extend_from_slice(format!("{i},ordinary_row_{i:04}\n").as_bytes());
            }
        }
        let path = fixture(&data);
        // Open with a clean copy first so bootstrap succeeds, then build checkpoints matching rows.
        let mut clean = Vec::new();
        clean.extend_from_slice(b"id,text\n");
        for i in 1..=100_u32 {
            if i == 5 {
                clean.extend_from_slice(b"5,good_trailing\n");
            } else if i == 30 {
                clean.extend_from_slice(b"30,needle_hit\n");
            } else {
                clean.extend_from_slice(format!("{i},ordinary_row_{i:04}\n").as_bytes());
            }
        }
        assert_eq!(clean.len(), data.len());
        fs::write(&path, &clean).unwrap();
        let session = Session::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let mut index = session
            .start_indexing(IndexConfig {
                chunk_bytes: 64,
                checkpoint_every: 20,
                memory_budget_bytes: 64 * 1024,
            })
            .unwrap()
            .wait()
            .unwrap();
        // Overwrite with same-length malformed row 5 (in stripe 0) while stripe 1 hits row 30 first.
        fs::write(&path, &data).unwrap();
        let mut session = session;
        session.source_stamp =
            crate::source_stamp::SourceStamp::from_file(&fs::File::open(&path).unwrap()).unwrap();
        index.set_provenance(
            session.source_stamp.clone(),
            session.dialect.delimiter,
            session.encoding,
        );

        let err = session
            .start_search_with_tuning(
                &index,
                b"needle_hit".to_vec(),
                SearchPosition { row: 1, column: 0 },
                BTreeMap::new(),
                CaseSensitivity::Sensitive,
                SearchTuning {
                    requested_threads: 4,
                    stripe_target_bytes: 256,
                    wait_for_later_hit_before_stripe_zero: true,
                },
            )
            .unwrap()
            .wait()
            .unwrap_err();
        assert!(
            matches!(err, QuarryError::Parse(_)),
            "preceding parse error must win over later stripe hit, got {err:?}"
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn parallel_search_respects_live_index_frontier_and_never_reports_false_not_found() {
        let mut data = Vec::new();
        data.extend_from_slice(b"id,text\n");
        for i in 1..=600_u32 {
            if i == 599 {
                data.extend_from_slice(b"599,tail_only_needle\n");
            } else {
                data.extend_from_slice(format!("{i},row_{i:05}\n").as_bytes());
            }
        }
        let path = fixture(&data);
        let session = Session::open(&path, OpenOptions::default()).unwrap();

        // Build a deterministic partial index stopping at row 200 (well before EOF).
        let mut partial = crate::StructuralIndex::new(IndexConfig {
            chunk_bytes: 64,
            checkpoint_every: 50,
            memory_budget_bytes: 64 * 1024,
        })
        .unwrap();
        partial.set_provenance(
            session.source_stamp.clone(),
            session.dialect.delimiter,
            session.encoding,
        );
        let mut boundaries = 0usize;
        for (idx, &byte) in data.iter().enumerate() {
            if byte == b'\n' {
                partial.record_boundary((idx + 1) as u64);
                boundaries += 1;
                if boundaries == 200 {
                    break;
                }
            }
        }
        assert_eq!(partial.indexed_rows(), 200);
        assert!(
            partial.indexed_bytes() < session.file_size,
            "partial snapshot must stop before EOF"
        );

        let search = session
            .start_search_with_tuning(
                &partial,
                b"tail_only_needle".to_vec(),
                SearchPosition { row: 1, column: 0 },
                BTreeMap::new(),
                CaseSensitivity::Sensitive,
                SearchTuning {
                    requested_threads: 4,
                    stripe_target_bytes: 256,
                    wait_for_later_hit_before_stripe_zero: false,
                },
            )
            .unwrap();
        let err = search.wait().unwrap_err();
        assert!(
            matches!(err, QuarryError::RowNotIndexed { .. }),
            "partial frontier without match must not report NotFound, got {err:?}"
        );

        // Once full index reaches verified EOF, absent needle returns NotFound and tail needle is found.
        let full = session
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            session
                .start_search_with_options(
                    &full,
                    b"definitely_absent".to_vec(),
                    SearchPosition { row: 1, column: 0 },
                    BTreeMap::new(),
                    CaseSensitivity::Sensitive,
                    Some(4),
                )
                .unwrap()
                .wait()
                .unwrap(),
            SearchOutcome::NotFound
        );
        assert!(matches!(
            session
                .start_search_with_options(
                    &full,
                    b"tail_only_needle".to_vec(),
                    SearchPosition { row: 1, column: 0 },
                    BTreeMap::new(),
                    CaseSensitivity::Sensitive,
                    Some(4),
                )
                .unwrap()
                .wait()
                .unwrap(),
            SearchOutcome::Match(m) if m.row == 599
        ));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_honors_cancellation_before_reading() {
        let path = fixture(b"a,b\n");
        let file = File::open(&path).unwrap();
        let shared = SharedState::new(fs::metadata(&path).unwrap().len());
        shared.cancelled.store(true, Ordering::Release);

        let outcome = run_search(
            file,
            b',',
            b"missing",
            CaseSensitivity::Sensitive,
            SearchPosition { row: 0, column: 0 },
            Checkpoint { row: 0, offset: 0 },
            &BTreeMap::new(),
            DEFAULT_MAX_RECORD_BYTES,
            crate::Encoding::Utf8,
            &shared,
        )
        .unwrap();

        assert_eq!(outcome, SearchOutcome::Cancelled);
        assert_eq!(shared.bytes_scanned.load(Ordering::Acquire), 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dropping_an_active_search_cancels_and_joins_its_worker() {
        let shared = Arc::new(SharedState::new(0));
        let worker_state = Arc::clone(&shared);
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let handle = thread::spawn(move || {
            let _completion = WorkerCompletion(&worker_state);
            while !worker_state.cancelled.load(Ordering::Acquire) {
                thread::yield_now();
            }
            worker_exited.store(true, Ordering::Release);
            Ok(SearchOutcome::Cancelled)
        });
        let job = SearchJob {
            shared: Arc::clone(&shared),
            handle: Some(handle),
        };

        drop(job);

        assert!(shared.cancelled.load(Ordering::Acquire));
        assert!(shared.done.load(Ordering::Acquire));
        assert!(exited.load(Ordering::Acquire));
    }

    #[test]
    fn search_rejects_records_over_the_bounded_cap() {
        let path = fixture(b"123456789,tail\n");
        let (session, index) = session_and_index(&path, false);
        let checkpoint = index.nearest_checkpoint(0);
        let job = SearchJob::start(
            session.path().to_path_buf(),
            session.file_size,
            session.dialect.delimiter,
            b"missing".to_vec(),
            CaseSensitivity::Sensitive,
            SearchPosition { row: 0, column: 0 },
            checkpoint,
            BTreeMap::new(),
            8,
            session.encoding,
        )
        .unwrap();
        while !job.progress().done {
            thread::yield_now();
        }
        assert!(job.error().unwrap().contains("8-byte limit"));
        assert!(matches!(
            job.wait(),
            Err(QuarryError::RecordTooLarge { limit: 8 })
        ));
        assert_eq!(DEFAULT_MAX_RECORD_BYTES, 64 * 1024 * 1024);

        assert!(matches!(
            SearchJob::start(
                session.path().to_path_buf(),
                session.file_size,
                session.dialect.delimiter,
                b"missing".to_vec(),
                CaseSensitivity::Sensitive,
                SearchPosition { row: 0, column: 0 },
                checkpoint,
                BTreeMap::from([((0, 0), vec![b'x'; 9])]),
                8,
                session.encoding,
            ),
            Err(QuarryError::RecordTooLarge { limit: 8 })
        ));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn parallel_search_streams_oversized_stripe_and_rejects_oversized_record_without_oom() {
        let mut data = Vec::new();
        data.extend_from_slice(b"id,note\n");
        for i in 1..=20_u32 {
            data.extend_from_slice(format!("{i:04},ok\n").as_bytes());
        }
        // Oversized record (200 bytes > max_record_bytes = 64) in a later stripe.
        data.extend_from_slice(b"0021,");
        data.extend(std::iter::repeat_n(b'x', 200));
        data.extend_from_slice(b"\n");
        let path = fixture(&data);
        let (session, _) = session_and_index(&path, true);
        let index = session
            .start_indexing(IndexConfig {
                chunk_bytes: 64,
                checkpoint_every: 10,
                memory_budget_bytes: 1024,
            })
            .unwrap()
            .wait()
            .unwrap();

        let job = SearchJob::start_plan(
            session.path().to_path_buf(),
            session.file_size,
            index.indexed_bytes(),
            index.indexed_rows(),
            None,
            session.dialect.delimiter,
            b"missing".to_vec(),
            CaseSensitivity::Sensitive,
            SearchPosition { row: 1, column: 0 },
            index.nearest_checkpoint(1),
            super::build_search_stripes(
                &index,
                index.nearest_checkpoint(1),
                index.indexed_bytes(),
                session.file_size,
                32,
            ),
            BTreeMap::new(),
            64,
            session.encoding,
            SearchTuning {
                requested_threads: 4,
                stripe_target_bytes: 32,
                wait_for_later_hit_before_stripe_zero: false,
            },
        )
        .unwrap();
        assert!(matches!(
            job.wait(),
            Err(QuarryError::RecordTooLarge { limit: 64 })
        ));
        fs::remove_file(path).unwrap();
    }
}
