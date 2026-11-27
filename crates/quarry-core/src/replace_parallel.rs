//! Parallel Replace All over a validated `StructuralIndex` with bounded streaming reorder
//! backpressure, zero-allocation verbatim fast path for non-matching rows, and exact
//! `CompletedIndex` emission.

use std::collections::BTreeMap;
use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
#[cfg(test)]
use std::time::Duration;

use quarry_delimited::{
    ByteEncodingClass, ScannerDialect, StructuralScanner, active_simd_backend, parse_record,
};

use super::{
    ExportConfig, ExportTarget, LiteralReplacement, PreparedSaveEdits, SharedState, UTF8_BOM,
    delimited_field_len,
};
use crate::case::ByteMatcher;
use crate::pool::{
    BudgetGuard, ByteBudget, JobExecutor, JobPriority, LARGE_RECORD_GATE_BYTES, LargeRecordGuard,
    engine_pool,
};
use crate::positional_io::{advise_readahead, read_at_exact_or_eof};
use crate::{IndexConfig, QuarryError, StructuralIndex};

pub(super) const DEFAULT_MIN_REPLACE_STRIPE_BYTES: u64 = 64 * 1024;
const MAX_REPLACE_STRIPE_BYTES: u64 = 16 * 1024 * 1024;
pub(super) const DEFAULT_REPLACE_FLUSH_CHUNK_BYTES: usize = 1024 * 1024;
pub(super) const DEFAULT_REPLACE_REORDER_BUDGET_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_HEAD_IN_FLIGHT: usize = 4;

#[derive(Clone, Debug)]
pub(super) struct ReplaceParallelTuning {
    pub(super) requested_workers: usize,
    pub(super) min_stripe_bytes: u64,
    pub(super) flush_chunk_bytes: usize,
    pub(super) reorder_budget_bytes: usize,
    pub(super) max_head_in_flight: usize,
    #[cfg(test)]
    pub(super) delay_stripe_zero: Option<Duration>,
    #[cfg(test)]
    pub(super) wait_stripe_zero_until: Option<Arc<AtomicBool>>,
    #[cfg(test)]
    pub(super) max_chunk_bytes_seen: Option<Arc<AtomicUsize>>,
    #[cfg(test)]
    pub(super) on_reorder_wait: Option<Arc<AtomicBool>>,
    #[cfg(test)]
    pub(super) max_non_head_output_bytes_seen: Option<Arc<AtomicUsize>>,
    #[cfg(test)]
    pub(super) large_gate_acquisitions: Option<Arc<AtomicUsize>>,
}

impl ReplaceParallelTuning {
    pub(super) fn for_production(requested_workers: usize) -> Self {
        Self {
            requested_workers,
            min_stripe_bytes: DEFAULT_MIN_REPLACE_STRIPE_BYTES,
            flush_chunk_bytes: DEFAULT_REPLACE_FLUSH_CHUNK_BYTES,
            reorder_budget_bytes: DEFAULT_REPLACE_REORDER_BUDGET_BYTES,
            max_head_in_flight: DEFAULT_MAX_HEAD_IN_FLIGHT,
            #[cfg(test)]
            delay_stripe_zero: None,
            #[cfg(test)]
            wait_stripe_zero_until: None,
            #[cfg(test)]
            max_chunk_bytes_seen: None,
            #[cfg(test)]
            on_reorder_wait: None,
            #[cfg(test)]
            max_non_head_output_bytes_seen: None,
            #[cfg(test)]
            large_gate_acquisitions: None,
        }
    }
}

pub(super) struct ReplaceParallelResult {
    pub(super) bytes_written: u64,
    pub(super) replacements: u64,
    pub(super) output_index: StructuralIndex,
    pub(super) effective_workers: usize,
}

#[derive(Clone, Copy)]
struct ReplaceStripe {
    id: usize,
    start_row: u64,
    end_row: u64,
    start_offset: u64,
    end_offset: u64,
    is_eof: bool,
}

pub(super) fn can_run_parallel_replace(
    index: &StructuralIndex,
    file_size: u64,
    tuning: &ReplaceParallelTuning,
) -> bool {
    tuning.requested_workers > 1 && build_replace_stripes(index, file_size, tuning).is_some()
}

fn build_replace_stripes(
    index: &StructuralIndex,
    file_size: u64,
    tuning: &ReplaceParallelTuning,
) -> Option<Vec<ReplaceStripe>> {
    if index.indexed_bytes() != file_size || index.indexed_rows() == 0 {
        return None;
    }
    let checkpoints = index.checkpoints();
    let first = checkpoints.first().copied()?;
    if first.row != 0 || first.offset != 0 {
        return None;
    }
    let target = (file_size / (tuning.requested_workers.saturating_mul(4).max(1) as u64))
        .clamp(tuning.min_stripe_bytes.max(1), MAX_REPLACE_STRIPE_BYTES);

    let mut stripes = Vec::new();
    let mut cur = first;
    for &cp in &checkpoints[1..] {
        if cp.offset <= cur.offset || cp.row <= cur.row || cp.offset > file_size {
            return None;
        }
        if cp.offset - cur.offset >= target {
            let id = stripes.len();
            stripes.push(ReplaceStripe {
                id,
                start_row: cur.row,
                end_row: cp.row,
                start_offset: cur.offset,
                end_offset: cp.offset,
                is_eof: cp.offset == file_size,
            });
            cur = cp;
        }
    }
    if cur.offset < file_size {
        let id = stripes.len();
        stripes.push(ReplaceStripe {
            id,
            start_row: cur.row,
            end_row: index.indexed_rows(),
            start_offset: cur.offset,
            end_offset: file_size,
            is_eof: true,
        });
    }
    (stripes.len() >= 2).then_some(stripes)
}

/// Bounds chunks in flight for the head-of-line stripe so even an expanding first stripe
/// streams to disk in bounded RAM without blocking on or starving non-head reorder budget.
struct HeadFlowControl {
    in_flight: Mutex<usize>,
    available: Condvar,
    max_in_flight: usize,
}

impl HeadFlowControl {
    fn new(max_in_flight: usize) -> Self {
        Self {
            in_flight: Mutex::new(0),
            available: Condvar::new(),
            max_in_flight: max_in_flight.max(1),
        }
    }

    fn acquire(&self, stop_flag: &AtomicBool) -> bool {
        let mut count = self.in_flight.lock().unwrap();
        while *count >= self.max_in_flight {
            if stop_flag.load(Ordering::Acquire) {
                return false;
            }
            let (guard, _) = self
                .available
                .wait_timeout(count, std::time::Duration::from_millis(2))
                .unwrap();
            count = guard;
        }
        if stop_flag.load(Ordering::Acquire) {
            return false;
        }
        *count = count.saturating_add(1);
        true
    }

    fn release(&self) {
        let mut count = self.in_flight.lock().unwrap();
        *count = count.saturating_sub(1);
        self.available.notify_all();
    }

    fn notify_all(&self) {
        self.available.notify_all();
    }
}

struct HeadFlowGuard {
    flow: Arc<HeadFlowControl>,
    active: bool,
}

impl HeadFlowGuard {
    fn acquire(flow: &Arc<HeadFlowControl>, stop_flag: &AtomicBool) -> Option<Self> {
        flow.acquire(stop_flag).then(|| Self {
            flow: Arc::clone(flow),
            active: true,
        })
    }

    fn disarm(mut self) {
        self.active = false;
    }
}

impl Drop for HeadFlowGuard {
    fn drop(&mut self) {
        if self.active {
            self.flow.release();
        }
    }
}

enum EmitCredit {
    Head(HeadFlowGuard),
    Reorder(BudgetGuard),
}

#[cfg(test)]
struct NonHeadOutputTracker {
    live: Arc<AtomicUsize>,
    max_seen: Arc<AtomicUsize>,
    bytes: usize,
}

#[cfg(test)]
impl NonHeadOutputTracker {
    fn new(live: &Arc<AtomicUsize>, max_seen: &Arc<AtomicUsize>) -> Self {
        Self {
            live: Arc::clone(live),
            max_seen: Arc::clone(max_seen),
            bytes: 0,
        }
    }

    fn record_bytes(&mut self, delta: usize) {
        if delta == 0 {
            return;
        }
        let now = self
            .live
            .fetch_add(delta, Ordering::AcqRel)
            .saturating_add(delta);
        self.max_seen.fetch_max(now, Ordering::AcqRel);
        self.bytes = self.bytes.saturating_add(delta);
    }
}

#[cfg(test)]
impl Drop for NonHeadOutputTracker {
    fn drop(&mut self) {
        if self.bytes > 0 {
            self.live.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

#[cfg(not(test))]
struct NonHeadOutputTracker;

#[cfg(not(test))]
impl NonHeadOutputTracker {
    fn record_bytes(&mut self, _delta: usize) {}
}

struct ReplaceOutputChunk {
    stripe_id: usize,
    seq: u64,
    is_last: bool,
    bytes: Vec<u8>,
    relative_record_ends: Vec<u64>,
    replacements: u64,
    max_width: usize,
    physical_rows: u64,
    is_head_credit: bool,
    // Drop `_non_head_tracker` (and `bytes`) BEFORE `_reorder_guard` releases budget credit
    // so waiting workers cannot allocate against the freed credit before the old chunk drops.
    _non_head_tracker: Option<NonHeadOutputTracker>,
    _reorder_guard: Option<BudgetGuard>,
}

enum StripeMessage {
    Chunk(ReplaceOutputChunk),
    Failed,
}

/// Replace all literal matches in a single decoded UTF-8 field using a precompiled `ByteMatcher`.
pub(super) fn replace_literal_with_matcher(
    field: &[u8],
    matcher: &ByteMatcher,
    replacement: &LiteralReplacement,
    max_record_bytes: usize,
) -> Result<(Vec<u8>, u64), QuarryError> {
    let mut count = 0_usize;
    let mut remainder = field;
    while let Some(position) = matcher.find(remainder) {
        count = count.saturating_add(1);
        remainder = &remainder[position + replacement.needle.len()..];
    }
    if count == 0 {
        return Ok((field.to_vec(), 0));
    }
    let output_len = field
        .len()
        .saturating_sub(replacement.needle.len().saturating_mul(count))
        .saturating_add(replacement.replacement.len().saturating_mul(count));
    if output_len > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }

    let mut output = Vec::with_capacity(output_len);
    let mut remainder = field;
    while let Some(position) = matcher.find(remainder) {
        output.extend_from_slice(&remainder[..position]);
        output.extend_from_slice(&replacement.replacement);
        remainder = &remainder[position + replacement.needle.len()..];
    }
    output.extend_from_slice(remainder);
    Ok((output, count as u64))
}

fn append_delimited_field(out: &mut Vec<u8>, field: &[u8], delimiter: u8, force_quotes: bool) {
    let needs_quotes = force_quotes
        || field
            .iter()
            .any(|byte| matches!(*byte, b'"' | b'\r' | b'\n') || *byte == delimiter);
    if !needs_quotes {
        out.extend_from_slice(field);
        return;
    }
    out.push(b'"');
    for &byte in field {
        out.push(byte);
        if byte == b'"' {
            out.push(b'"');
        }
    }
    out.push(b'"');
}

fn append_serialized_fields(
    out: &mut Vec<u8>,
    prefix: &[u8],
    fields: &[Vec<u8>],
    delimiter: u8,
    ending: &[u8],
    is_first_record: bool,
    max_record_bytes: usize,
) -> Result<u64, QuarryError> {
    let serialized_len = fields.iter().enumerate().fold(
        prefix
            .len()
            .saturating_add(ending.len())
            .saturating_add(fields.len().saturating_sub(1)),
        |length, (column, field)| {
            let force_quotes = (is_first_record
                && prefix.is_empty()
                && column == 0
                && field.starts_with(UTF8_BOM))
                || (fields.len() == 1 && field.is_empty() && ending.is_empty());
            length.saturating_add(delimited_field_len(field, delimiter, force_quotes))
        },
    );
    if serialized_len > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }
    out.reserve(serialized_len);
    out.extend_from_slice(prefix);
    for (column, field) in fields.iter().enumerate() {
        if column > 0 {
            out.push(delimiter);
        }
        let force_quotes =
            (is_first_record && prefix.is_empty() && column == 0 && field.starts_with(UTF8_BOM))
                || (fields.len() == 1 && field.is_empty() && ending.is_empty());
        append_delimited_field(out, field, delimiter, force_quotes);
    }
    out.extend_from_slice(ending);
    Ok(serialized_len as u64)
}

fn append_rewritten_header(
    out: &mut Vec<u8>,
    record: &[u8],
    delimiter: u8,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    max_record_bytes: usize,
) -> Result<u64, QuarryError> {
    let (prefix, body) = record
        .strip_prefix(UTF8_BOM)
        .map_or((&[][..], record), |stripped| (UTF8_BOM, stripped));
    let fields = parse_record(body, delimiter)?;
    if header_renames
        .last_key_value()
        .is_some_and(|(column, _)| *column >= fields.len())
    {
        return Err(QuarryError::InvalidOption(
            "header rename column is out of range",
        ));
    }
    let ending = if body.ends_with(b"\r\n") {
        b"\r\n".as_slice()
    } else if body.ends_with(b"\n") {
        b"\n".as_slice()
    } else {
        b"".as_slice()
    };
    let mut values = Vec::with_capacity(fields.len());
    for (column, field) in fields.iter().enumerate() {
        let value = header_renames
            .get(&column)
            .map_or_else(|| field.as_ref(), Vec::as_slice);
        values.push(value.to_vec());
    }
    append_serialized_fields(
        out,
        prefix,
        &values,
        delimiter,
        ending,
        true,
        max_record_bytes,
    )
}

/// Process one UTF-8 record for Replace All, appending its output bytes to `out` and returning
/// the replacement count. When `needle_contains_quote` is false and the row has no cell edits,
/// a single SIMD/memmem `matcher.find` check on the raw record bypasses `parse_record` and all
/// per-field allocations on non-matching rows.
#[allow(clippy::too_many_arguments)]
fn append_replaced_record(
    out: &mut Vec<u8>,
    record: &[u8],
    row_number: u64,
    delimiter: u8,
    has_header: bool,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    matcher: &ByteMatcher,
    replacement: &LiteralReplacement,
    needle_contains_quote: bool,
    max_record_bytes: usize,
) -> Result<u64, QuarryError> {
    if record.len() > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }
    if has_header && row_number == 0 {
        if header_renames.is_empty() {
            out.extend_from_slice(record);
        } else {
            append_rewritten_header(out, record, delimiter, header_renames, max_record_bytes)?;
        }
        return Ok(0);
    }

    let original = record;
    let (prefix, body) = if row_number == 0 {
        record
            .strip_prefix(UTF8_BOM)
            .map_or((&[][..], record), |stripped| (UTF8_BOM, stripped))
    } else {
        (&[][..], record)
    };
    let has_cell_edits = !cell_edits.is_empty()
        && cell_edits
            .range((row_number, 0)..=(row_number, usize::MAX))
            .next()
            .is_some();

    // Exact zero-allocation verbatim fast path: if the needle contains no quote byte (`"`),
    // RFC 4180 unescaping (`""` -> `"`) can never manufacture a match that is absent from
    // the raw record bytes, and `StructuralScanner` already validated RFC 4180 syntax.
    if !has_cell_edits && !needle_contains_quote && matcher.find(body).is_none() {
        out.extend_from_slice(original);
        return Ok(0);
    }

    let ending = if body.ends_with(b"\r\n") {
        b"\r\n".as_slice()
    } else if body.ends_with(b"\n") {
        b"\n".as_slice()
    } else {
        b"".as_slice()
    };
    let fields = parse_record(body, delimiter)?;
    if has_cell_edits
        && cell_edits
            .range((row_number, 0)..=(row_number, usize::MAX))
            .next_back()
            .is_some_and(|((_, column), _)| *column >= fields.len())
    {
        return Err(QuarryError::InvalidOption(
            "cell edit column is out of range",
        ));
    }

    let mut replacements = 0_u64;
    let mut decoded_bytes = 0_usize;
    let mut replaced_fields = Vec::with_capacity(fields.len());
    for (column, field) in fields.iter().enumerate() {
        let value = if has_cell_edits {
            cell_edits
                .get(&(row_number, column))
                .map_or_else(|| field.as_ref(), Vec::as_slice)
        } else {
            field.as_ref()
        };
        let (value, count) =
            replace_literal_with_matcher(value, matcher, replacement, max_record_bytes)?;
        decoded_bytes = decoded_bytes.saturating_add(value.len());
        if decoded_bytes > max_record_bytes {
            return Err(QuarryError::RecordTooLarge {
                limit: max_record_bytes,
            });
        }
        replacements = replacements.saturating_add(count);
        replaced_fields.push(value);
    }

    if replacements == 0 && !has_cell_edits {
        out.extend_from_slice(original);
        return Ok(0);
    }

    append_serialized_fields(
        out,
        prefix,
        &replaced_fields,
        delimiter,
        ending,
        row_number == 0,
        max_record_bytes,
    )?;
    Ok(replacements)
}

const FIELD_DESCRIPTOR_BYTES: usize =
    std::mem::size_of::<std::borrow::Cow<'static, [u8]>>() + std::mem::size_of::<Vec<u8>>();

/// Estimate `(upper_bound_output_bytes, upper_bound_working_bytes)` for replacing one record.
///
/// Non-matching unedited rows taking the verbatim fast path return `(rec.len(), rec.len())` with
/// zero extra scanning. Parsed rows account for both expanded output bytes and the temporary
/// `Vec<Cow<[u8]>>` + `Vec<Vec<u8>>` field descriptors allocated inside `append_replaced_record`.
#[allow(clippy::too_many_arguments)]
fn estimate_replace_memory(
    rec: &[u8],
    delimiter: u8,
    row_number: u64,
    has_header: bool,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    matcher: &ByteMatcher,
    needle_contains_quote: bool,
    expansion_ratio: usize,
    max_record_bytes: usize,
) -> (usize, usize) {
    let is_header = has_header && row_number == 0;
    let has_cell_edits = !is_header
        && cell_edits
            .range((row_number, 0)..=(row_number, usize::MAX))
            .next()
            .is_some();
    let takes_verbatim_fast_path = if is_header {
        header_renames.is_empty()
    } else {
        !has_cell_edits && !needle_contains_quote && matcher.find(rec).is_none()
    };
    if takes_verbatim_fast_path {
        return (rec.len(), rec.len());
    }
    let edit_bytes: usize = if is_header {
        header_renames.values().map(Vec::len).sum()
    } else if has_cell_edits {
        cell_edits
            .range((row_number, 0)..=(row_number, usize::MAX))
            .map(|(_, value)| value.len())
            .sum()
    } else {
        0
    };
    let field_count = memchr::memchr_iter(delimiter, rec)
        .count()
        .saturating_add(1);
    let est_output_bytes = rec
        .len()
        .saturating_add(edit_bytes)
        .saturating_mul(expansion_ratio)
        .saturating_mul(2)
        // Account for up to two enclosing quotes per field when quoting is added.
        .saturating_add(field_count.saturating_mul(2))
        .saturating_add(16)
        .min(max_record_bytes);
    let field_descriptor_bytes = field_count.saturating_mul(FIELD_DESCRIPTOR_BYTES);
    let est_working_bytes = rec
        .len()
        .saturating_add(est_output_bytes)
        .saturating_add(field_descriptor_bytes);
    (est_output_bytes, est_working_bytes)
}

fn clear_record_buffer(record: &mut Vec<u8>) {
    record.clear();
    if record.capacity() > LARGE_RECORD_GATE_BYTES {
        record.shrink_to(0);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn copy_with_replaced_records_parallel(
    source: File,
    output: &mut ExportTarget,
    delimiter: u8,
    has_header: bool,
    edits: &PreparedSaveEdits,
    config: ExportConfig,
    source_index: &StructuralIndex,
    tuning: &ReplaceParallelTuning,
    shared: &Arc<SharedState>,
) -> Result<Option<ReplaceParallelResult>, QuarryError> {
    let replacement = edits
        .replacement
        .as_ref()
        .expect("parallel replace requires a literal replacement");
    let stripes = build_replace_stripes(source_index, shared.total_bytes, tuning)
        .expect("caller verified parallel replace eligibility");
    let worker_count = tuning.requested_workers.min(stripes.len()).max(1);
    let executor = JobExecutor::new(engine_pool(), JobPriority::Foreground, worker_count)?;
    let actual_workers = executor.effective_workers().min(stripes.len()).max(1);

    let source = Arc::new(source);
    let stripes = Arc::new(stripes);
    let next_stripe = Arc::new(AtomicUsize::new(0));
    let head_stripe_id = Arc::new(AtomicUsize::new(0));
    #[cfg(test)]
    let reorder_budget = Arc::new(ByteBudget::with_wait_signal(
        tuning.reorder_budget_bytes.max(1),
        tuning.on_reorder_wait.clone(),
    ));
    #[cfg(not(test))]
    let reorder_budget = Arc::new(ByteBudget::new(tuning.reorder_budget_bytes.max(1)));
    let head_flow = Arc::new(HeadFlowControl::new(tuning.max_head_in_flight));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let earliest_error_row = Arc::new(AtomicU64::new(u64::MAX));
    let earliest_error: Arc<Mutex<Option<(u64, QuarryError)>>> = Arc::new(Mutex::new(None));
    let header_renames = Arc::new(edits.headers.clone());
    let cell_edits = Arc::new(edits.cells.clone());
    let replacement = Arc::new(replacement.clone());
    let needle_contains_quote = replacement.needle.contains(&b'"');
    let expansion_ratio = replacement
        .replacement
        .len()
        .div_ceil(replacement.needle.len().max(1))
        .max(1);
    let flush_chunk_bytes = tuning.flush_chunk_bytes.max(1);
    let large_record_gate = Arc::clone(engine_pool().large_record_gate());
    let dialect = ScannerDialect::rfc4180(delimiter);
    let simd_backend = active_simd_backend();
    #[cfg(test)]
    let delay_stripe_zero = tuning.delay_stripe_zero;
    #[cfg(test)]
    let wait_stripe_zero_until = tuning.wait_stripe_zero_until.clone();
    #[cfg(test)]
    let max_chunk_bytes_seen = tuning.max_chunk_bytes_seen.clone();
    #[cfg(test)]
    let max_non_head_output_bytes_seen = tuning.max_non_head_output_bytes_seen.clone();
    #[cfg(test)]
    let live_non_head_output_bytes = Arc::new(AtomicUsize::new(0));
    #[cfg(test)]
    let large_gate_acquisitions = tuning.large_gate_acquisitions.clone();

    let (tx, rx) = mpsc::channel::<StripeMessage>();

    for _ in 0..actual_workers {
        let source = Arc::clone(&source);
        let stripes = Arc::clone(&stripes);
        let next_stripe = Arc::clone(&next_stripe);
        let head_stripe_id = Arc::clone(&head_stripe_id);
        let reorder_budget = Arc::clone(&reorder_budget);
        let head_flow = Arc::clone(&head_flow);
        let stop_flag = Arc::clone(&stop_flag);
        let earliest_error_row = Arc::clone(&earliest_error_row);
        let earliest_error = Arc::clone(&earliest_error);
        let header_renames = Arc::clone(&header_renames);
        let cell_edits = Arc::clone(&cell_edits);
        let replacement = Arc::clone(&replacement);
        let large_record_gate = Arc::clone(&large_record_gate);
        let shared_worker = Arc::clone(shared);
        let tx = tx.clone();
        #[cfg(test)]
        let max_chunk_bytes_seen = max_chunk_bytes_seen.clone();
        #[cfg(test)]
        let wait_stripe_zero_until = wait_stripe_zero_until.clone();
        #[cfg(test)]
        let max_non_head_output_bytes_seen = max_non_head_output_bytes_seen.clone();
        #[cfg(test)]
        let live_non_head_output_bytes = Arc::clone(&live_non_head_output_bytes);
        #[cfg(test)]
        let large_gate_acquisitions = large_gate_acquisitions.clone();

        let spawned = executor.spawn(&shared.cancel_requested, move || {
            let shared = shared_worker;
            let matcher = ByteMatcher::new(&replacement.needle, replacement.case_sensitivity);
            let mut read_buf = vec![0_u8; config.chunk_bytes.max(1)];
            let mut record = Vec::new();
            let mut large_guard: Option<LargeRecordGuard> = None;

            let record_error = |row: u64, error: QuarryError| {
                earliest_error_row.fetch_min(row, Ordering::AcqRel);
                let mut slot = earliest_error.lock().unwrap();
                if slot.as_ref().is_none_or(|(best_row, _)| row < *best_row) {
                    *slot = Some((row, error));
                }
                let _ = tx.send(StripeMessage::Failed);
            };

            let should_stop = |row: u64| -> bool {
                stop_flag.load(Ordering::Acquire)
                    || shared.cancel_requested.load(Ordering::Acquire)
                    || row >= earliest_error_row.load(Ordering::Acquire)
            };

            'worker: loop {
                if stop_flag.load(Ordering::Acquire)
                    || shared.cancel_requested.load(Ordering::Acquire)
                {
                    break 'worker;
                }
                let stripe_idx = next_stripe.fetch_add(1, Ordering::AcqRel);
                if stripe_idx >= stripes.len() {
                    break 'worker;
                }
                let stripe = stripes[stripe_idx];
                if stripe.start_row >= earliest_error_row.load(Ordering::Acquire) {
                    break 'worker;
                }

                #[cfg(test)]
                if stripe.id == 0 {
                    if let Some(signal) = &wait_stripe_zero_until {
                        let deadline = std::time::Instant::now() + Duration::from_secs(2);
                        while !signal.load(Ordering::Acquire)
                            && !stop_flag.load(Ordering::Acquire)
                            && !shared.cancel_requested.load(Ordering::Acquire)
                            && std::time::Instant::now() < deadline
                        {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                    if let Some(delay) = delay_stripe_zero {
                        std::thread::sleep(delay);
                    }
                }

                advise_readahead(
                    &source,
                    stripe.start_offset,
                    stripe.end_offset.saturating_sub(stripe.start_offset),
                );
                let mut scanner = if stripe.id == 0 {
                    match StructuralScanner::<true>::with_dialect_and_backend(
                        dialect,
                        0,
                        ByteEncodingClass::Utf8,
                        simd_backend,
                    ) {
                        Ok(scanner) => scanner,
                        Err(err) => {
                            record_error(stripe.start_row, err.into());
                            break 'worker;
                        }
                    }
                } else {
                    match StructuralScanner::<true>::with_dialect_without_bom(
                        dialect,
                        stripe.start_offset,
                        ByteEncodingClass::Utf8,
                    ) {
                        Ok(scanner) => scanner,
                        Err(err) => {
                            record_error(stripe.start_row, err.into());
                            break 'worker;
                        }
                    }
                };

                clear_record_buffer(&mut record);
                let mut pos = stripe.start_offset;
                let mut physical_row = stripe.start_row;
                let mut chunk_bytes = Vec::new();
                let mut relative_ends = Vec::new();
                let mut chunk_replacements = 0_u64;
                let mut chunk_rows = 0_u64;
                let mut seq = 0_u64;

                let acquire_emit_credit = |footprint: usize| -> Option<EmitCredit> {
                    if stop_flag.load(Ordering::Acquire)
                        || shared.cancel_requested.load(Ordering::Acquire)
                    {
                        return None;
                    }
                    if stripe.id == head_stripe_id.load(Ordering::Acquire) {
                        return HeadFlowGuard::acquire(&head_flow, &stop_flag)
                            .map(EmitCredit::Head);
                    }
                    let guard = reorder_budget.acquire_or_bypass(footprint, &stop_flag, || {
                        stripe.id == head_stripe_id.load(Ordering::Acquire)
                            || shared.cancel_requested.load(Ordering::Acquire)
                            || earliest_error_row.load(Ordering::Acquire) != u64::MAX
                    })?;
                    if stop_flag.load(Ordering::Acquire)
                        || shared.cancel_requested.load(Ordering::Acquire)
                    {
                        return None;
                    }
                    if stripe.id == head_stripe_id.load(Ordering::Acquire) {
                        // Bypassed because this stripe became the head stripe; release reorder
                        // budget and bound by `head_flow` instead.
                        drop(guard);
                        return HeadFlowGuard::acquire(&head_flow, &stop_flag)
                            .map(EmitCredit::Head);
                    }
                    Some(EmitCredit::Reorder(guard))
                };

                let emit_chunk_with_credit = |is_last: bool,
                                              pre_credit: Option<EmitCredit>,
                                              pre_tracker: Option<NonHeadOutputTracker>,
                                              chunk_bytes: &mut Vec<u8>,
                                              relative_ends: &mut Vec<u64>,
                                              chunk_replacements: &mut u64,
                                              chunk_rows: &mut u64,
                                              seq: &mut u64,
                                              max_width: usize|
                 -> bool {
                    if stop_flag.load(Ordering::Acquire)
                        || shared.cancel_requested.load(Ordering::Acquire)
                    {
                        chunk_bytes.clear();
                        relative_ends.clear();
                        return false;
                    }
                    if earliest_error_row.load(Ordering::Acquire) != u64::MAX {
                        // Output will not be published; keep scanning earlier rows for the
                        // earliest deterministic error without buffering output bytes.
                        chunk_bytes.clear();
                        relative_ends.clear();
                        return true;
                    }
                    let mut bytes = std::mem::take(chunk_bytes);
                    #[cfg(test)]
                    if let Some(tracker) = &max_chunk_bytes_seen {
                        tracker.fetch_max(bytes.len(), Ordering::AcqRel);
                    }
                    let mut ends = std::mem::take(relative_ends);
                    let replacements = std::mem::take(chunk_replacements);
                    let physical_rows = std::mem::take(chunk_rows);
                    let allocated_footprint = |bytes: &Vec<u8>, ends: &Vec<u64>| {
                        bytes
                            .capacity()
                            .saturating_add(
                                ends.capacity().saturating_mul(std::mem::size_of::<u64>()),
                            )
                            .max(1)
                    };

                    let credit = match pre_credit {
                        Some(mut credit) => {
                            if let EmitCredit::Reorder(ref mut guard) = credit {
                                if allocated_footprint(&bytes, &ends) > guard.bytes()
                                    || bytes.capacity().saturating_sub(bytes.len()) > 64 * 1024
                                {
                                    bytes.shrink_to_fit();
                                    ends.shrink_to_fit();
                                }
                                guard.shrink_to(allocated_footprint(&bytes, &ends));
                            }
                            credit
                        }
                        None => {
                            debug_assert!(bytes.is_empty() && ends.is_empty());
                            let Some(credit) =
                                acquire_emit_credit(allocated_footprint(&bytes, &ends))
                            else {
                                return false;
                            };
                            credit
                        }
                    };

                    let (is_head_credit, reorder_guard) = match credit {
                        EmitCredit::Head(guard) => {
                            guard.disarm();
                            (true, None)
                        }
                        EmitCredit::Reorder(guard) => (false, Some(guard)),
                    };

                    let current_seq = *seq;
                    *seq = seq.saturating_add(1);
                    let msg = StripeMessage::Chunk(ReplaceOutputChunk {
                        stripe_id: stripe.id,
                        seq: current_seq,
                        is_last,
                        bytes,
                        relative_record_ends: ends,
                        replacements,
                        max_width,
                        physical_rows,
                        is_head_credit,
                        _reorder_guard: reorder_guard,
                        _non_head_tracker: pre_tracker,
                    });
                    if tx.send(msg).is_err() {
                        if is_head_credit {
                            head_flow.release();
                        }
                        return false;
                    }
                    true
                };

                let mut chunk_credit: Option<EmitCredit> = None;
                let mut chunk_tracker: Option<NonHeadOutputTracker> = None;

                let process_record = |rec: &[u8],
                                      row_num: u64,
                                      chunk_bytes: &mut Vec<u8>,
                                      relative_ends: &mut Vec<u64>,
                                      chunk_replacements: &mut u64,
                                      chunk_rows: &mut u64,
                                      seq: &mut u64,
                                      large_guard: &mut Option<LargeRecordGuard>,
                                      chunk_credit: &mut Option<EmitCredit>,
                                      chunk_tracker: &mut Option<NonHeadOutputTracker>|
                 -> Result<bool, QuarryError> {
                    let (est_output_bytes, est_working_bytes) = estimate_replace_memory(
                        rec,
                        delimiter,
                        row_num,
                        has_header,
                        &header_renames,
                        &cell_edits,
                        &matcher,
                        needle_contains_quote,
                        expansion_ratio,
                        config.max_record_bytes,
                    );
                    let needs_large_gate = rec.len() > LARGE_RECORD_GATE_BYTES
                        || est_output_bytes > LARGE_RECORD_GATE_BYTES
                        || est_working_bytes > LARGE_RECORD_GATE_BYTES;
                    let row_footprint = est_output_bytes
                        .saturating_add(std::mem::size_of::<u64>())
                        .max(1);

                    // Plan exact `chunk_bytes` and `relative_ends` capacities inside a reorder
                    // guard's reserved bytes so neither `append_replaced_record` nor `push` can
                    // trigger unbudgeted Vec growth past `guard.bytes()`.
                    let plan_chunk_capacities = |chunk_bytes: &Vec<u8>,
                                                 relative_ends: &Vec<u64>,
                                                 budget: usize|
                     -> Option<(usize, usize)> {
                        let need_bytes = chunk_bytes.len().saturating_add(est_output_bytes);
                        let need_ends = relative_ends.len().saturating_add(1);
                        let cur_bytes_cap = chunk_bytes.capacity();
                        let cur_ends_cap = relative_ends.capacity();
                        let next_ends_cap = if need_ends <= cur_ends_cap {
                            cur_ends_cap
                        } else {
                            let min_bytes_cap = cur_bytes_cap.max(need_bytes);
                            let max_ends =
                                budget.saturating_sub(min_bytes_cap) / std::mem::size_of::<u64>();
                            if need_ends > max_ends {
                                return None;
                            }
                            cur_ends_cap
                                .saturating_mul(2)
                                .max(16)
                                .min(max_ends)
                                .max(need_ends)
                        };
                        let ends_cap_bytes =
                            next_ends_cap.saturating_mul(std::mem::size_of::<u64>());
                        let max_bytes = budget.saturating_sub(ends_cap_bytes);
                        let next_bytes_cap = if need_bytes <= cur_bytes_cap {
                            if cur_bytes_cap > max_bytes {
                                return None;
                            }
                            cur_bytes_cap
                        } else {
                            if need_bytes > max_bytes {
                                return None;
                            }
                            cur_bytes_cap
                                .saturating_mul(2)
                                .max(need_bytes)
                                .min(max_bytes)
                        };
                        Some((next_bytes_cap, next_ends_cap))
                    };

                    // Flush any buffered output BEFORE acquiring fresh credit or `large_record_gate`
                    // whenever this row's capacity plan would exceed our pre-acquired `chunk_credit`
                    // (or when we need `large_record_gate`). Because `chunk_credit` is already held,
                    // flushing is non-blocking and leaves `chunk_bytes` empty before any potential wait.
                    let would_exceed_credit = match chunk_credit.as_ref() {
                        Some(EmitCredit::Reorder(guard)) => {
                            plan_chunk_capacities(chunk_bytes, relative_ends, guard.bytes())
                                .is_none()
                        }
                        Some(EmitCredit::Head(_)) => {
                            !chunk_bytes.is_empty()
                                && chunk_bytes.len().saturating_add(est_output_bytes)
                                    > flush_chunk_bytes
                        }
                        None => false,
                    };
                    if !chunk_bytes.is_empty() && (would_exceed_credit || needs_large_gate) {
                        let credit = chunk_credit.take();
                        let tracker = chunk_tracker.take();
                        if !emit_chunk_with_credit(
                            false,
                            credit,
                            tracker,
                            chunk_bytes,
                            relative_ends,
                            chunk_replacements,
                            chunk_rows,
                            seq,
                            0,
                        ) {
                            return Ok(false);
                        }
                    }

                    // Lock ordering: always acquire `reorder_budget` / `head_flow` BEFORE
                    // `large_record_gate` and BEFORE writing any output bytes into `chunk_bytes`.
                    // A worker waiting on `reorder_budget` therefore holds 0 retained output bytes
                    // and never holds `large_record_gate`.
                    if chunk_credit.is_none() {
                        debug_assert!(chunk_bytes.is_empty());
                        debug_assert!(large_guard.is_none());
                        let reserve = if row_footprint >= flush_chunk_bytes {
                            row_footprint
                        } else {
                            flush_chunk_bytes
                                .min(reorder_budget.limit())
                                .max(row_footprint)
                        };
                        let Some(credit) = acquire_emit_credit(reserve) else {
                            return Ok(false);
                        };
                        #[cfg(test)]
                        if matches!(credit, EmitCredit::Reorder(_))
                            && let Some(max_seen) = &max_non_head_output_bytes_seen
                        {
                            *chunk_tracker = Some(NonHeadOutputTracker::new(
                                &live_non_head_output_bytes,
                                max_seen,
                            ));
                        }
                        *chunk_credit = Some(credit);
                    }
                    if let Some(EmitCredit::Reorder(guard)) = chunk_credit.as_ref()
                        && let Some((next_bytes_cap, next_ends_cap)) =
                            plan_chunk_capacities(chunk_bytes, relative_ends, guard.bytes())
                    {
                        if next_bytes_cap > chunk_bytes.capacity() {
                            chunk_bytes.reserve_exact(next_bytes_cap - chunk_bytes.len());
                        }
                        if next_ends_cap > relative_ends.capacity() {
                            relative_ends.reserve_exact(next_ends_cap - relative_ends.len());
                        }
                    }
                    if needs_large_gate && large_guard.is_none() {
                        let guard = if let Some(g) = large_record_gate.try_acquire() {
                            g
                        } else if let Some(g) = large_record_gate.acquire(&shared.cancel_requested)
                        {
                            g
                        } else {
                            return Ok(false);
                        };
                        #[cfg(test)]
                        if let Some(counter) = &large_gate_acquisitions {
                            counter.fetch_add(1, Ordering::AcqRel);
                        }
                        *large_guard = Some(guard);
                    }

                    let before_len = chunk_bytes.len();
                    let appended = append_replaced_record(
                        chunk_bytes,
                        rec,
                        row_num,
                        delimiter,
                        has_header,
                        &header_renames,
                        &cell_edits,
                        &matcher,
                        &replacement,
                        needle_contains_quote,
                        config.max_record_bytes,
                    );

                    let count = match appended {
                        Ok(count) => count,
                        Err(err) => {
                            chunk_bytes.clear();
                            relative_ends.clear();
                            chunk_tracker.take();
                            chunk_credit.take();
                            return Err(err);
                        }
                    };
                    if let Some(tracker) = chunk_tracker.as_mut() {
                        tracker.record_bytes(chunk_bytes.len().saturating_sub(before_len));
                    }
                    *chunk_replacements = chunk_replacements.saturating_add(count);
                    *chunk_rows = chunk_rows.saturating_add(1);
                    relative_ends.push(chunk_bytes.len() as u64);

                    if chunk_bytes.len() >= flush_chunk_bytes || row_footprint >= flush_chunk_bytes
                    {
                        let credit = chunk_credit.take();
                        let tracker = chunk_tracker.take();
                        if !emit_chunk_with_credit(
                            false,
                            credit,
                            tracker,
                            chunk_bytes,
                            relative_ends,
                            chunk_replacements,
                            chunk_rows,
                            seq,
                            0,
                        ) {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                };

                while pos < stripe.end_offset {
                    if should_stop(physical_row) {
                        break 'worker;
                    }
                    let want = (stripe.end_offset - pos).min(read_buf.len() as u64) as usize;
                    let read = match read_at_exact_or_eof(&source, pos, &mut read_buf[..want]) {
                        Ok(n) => n,
                        Err(err) => {
                            record_error(physical_row, err.into());
                            break 'worker;
                        }
                    };
                    if read < want {
                        record_error(physical_row, QuarryError::SourceChanged);
                        break 'worker;
                    }

                    let mut segment_start = 0_usize;
                    let mut deferred_error: Option<(u64, QuarryError)> = None;
                    let mut stopped = false;

                    let scan_result = scanner.scan_chunk(&read_buf[..read], pos, |absolute_end| {
                        let local_end = (absolute_end - pos) as usize;
                        if deferred_error.is_none() && !stopped {
                            if should_stop(physical_row) {
                                stopped = true;
                            } else {
                                let slice = &read_buf[segment_start..local_end];
                                let total_len = record.len().saturating_add(slice.len());
                                if total_len > config.max_record_bytes {
                                    deferred_error = Some((
                                        physical_row,
                                        QuarryError::RecordTooLarge {
                                            limit: config.max_record_bytes,
                                        },
                                    ));
                                } else {
                                    let rec: &[u8] = if record.is_empty() {
                                        slice
                                    } else {
                                        record.extend_from_slice(slice);
                                        &record
                                    };
                                    match process_record(
                                        rec,
                                        physical_row,
                                        &mut chunk_bytes,
                                        &mut relative_ends,
                                        &mut chunk_replacements,
                                        &mut chunk_rows,
                                        &mut seq,
                                        &mut large_guard,
                                        &mut chunk_credit,
                                        &mut chunk_tracker,
                                    ) {
                                        Ok(true) => {}
                                        Ok(false) => stopped = true,
                                        Err(err) => deferred_error = Some((physical_row, err)),
                                    }
                                }
                            }
                        }
                        if !record.is_empty() {
                            clear_record_buffer(&mut record);
                        }
                        large_guard.take();
                        if chunk_bytes.is_empty() {
                            chunk_tracker.take();
                            chunk_credit.take();
                        }
                        physical_row = physical_row.saturating_add(1);
                        segment_start = local_end;
                    });

                    pos = pos.saturating_add(read as u64);
                    shared
                        .bytes_scanned
                        .fetch_add(read as u64, Ordering::Relaxed);

                    if let Some((err_row, err)) = deferred_error {
                        record_error(err_row, err);
                        break 'worker;
                    }
                    if stopped || should_stop(physical_row) {
                        break 'worker;
                    }
                    if let Err(err) = scan_result {
                        record_error(physical_row, err.into());
                        break 'worker;
                    }

                    if chunk_bytes.len() >= flush_chunk_bytes {
                        let credit = chunk_credit.take();
                        let tracker = chunk_tracker.take();
                        if !emit_chunk_with_credit(
                            false,
                            credit,
                            tracker,
                            &mut chunk_bytes,
                            &mut relative_ends,
                            &mut chunk_replacements,
                            &mut chunk_rows,
                            &mut seq,
                            scanner.maximum_width(),
                        ) {
                            break 'worker;
                        }
                    }

                    let remainder = &read_buf[segment_start..read];
                    if !remainder.is_empty() {
                        if record.len().saturating_add(remainder.len()) > config.max_record_bytes {
                            record_error(
                                physical_row,
                                QuarryError::RecordTooLarge {
                                    limit: config.max_record_bytes,
                                },
                            );
                            break 'worker;
                        }
                        if record.len().saturating_add(remainder.len()) > LARGE_RECORD_GATE_BYTES
                            && large_guard.is_none()
                        {
                            if !chunk_bytes.is_empty() {
                                let credit = chunk_credit.take();
                                let tracker = chunk_tracker.take();
                                if !emit_chunk_with_credit(
                                    false,
                                    credit,
                                    tracker,
                                    &mut chunk_bytes,
                                    &mut relative_ends,
                                    &mut chunk_replacements,
                                    &mut chunk_rows,
                                    &mut seq,
                                    scanner.maximum_width(),
                                ) {
                                    break 'worker;
                                }
                            }
                            if chunk_credit.is_none() {
                                let reserve = config
                                    .max_record_bytes
                                    .saturating_add(std::mem::size_of::<u64>())
                                    .max(1);
                                let Some(credit) = acquire_emit_credit(reserve) else {
                                    break 'worker;
                                };
                                #[cfg(test)]
                                if matches!(credit, EmitCredit::Reorder(_))
                                    && let Some(max_seen) = &max_non_head_output_bytes_seen
                                {
                                    chunk_tracker = Some(NonHeadOutputTracker::new(
                                        &live_non_head_output_bytes,
                                        max_seen,
                                    ));
                                }
                                chunk_credit = Some(credit);
                            }
                            let guard = if let Some(g) = large_record_gate.try_acquire() {
                                g
                            } else if let Some(g) =
                                large_record_gate.acquire(&shared.cancel_requested)
                            {
                                g
                            } else {
                                break 'worker;
                            };
                            #[cfg(test)]
                            if let Some(counter) = &large_gate_acquisitions {
                                counter.fetch_add(1, Ordering::AcqRel);
                            }
                            large_guard = Some(guard);
                        }
                        record.extend_from_slice(remainder);
                    }
                }

                if stripe.is_eof {
                    let mut deferred_error: Option<(u64, QuarryError)> = None;
                    let mut stopped = false;
                    let finish_result = scanner.finish(pos, |_| {
                        if deferred_error.is_none() && !stopped {
                            if should_stop(physical_row) {
                                stopped = true;
                            } else {
                                match process_record(
                                    &record,
                                    physical_row,
                                    &mut chunk_bytes,
                                    &mut relative_ends,
                                    &mut chunk_replacements,
                                    &mut chunk_rows,
                                    &mut seq,
                                    &mut large_guard,
                                    &mut chunk_credit,
                                    &mut chunk_tracker,
                                ) {
                                    Ok(true) => {}
                                    Ok(false) => stopped = true,
                                    Err(err) => deferred_error = Some((physical_row, err)),
                                }
                            }
                        }
                        clear_record_buffer(&mut record);
                        large_guard.take();
                        if chunk_bytes.is_empty() {
                            chunk_tracker.take();
                            chunk_credit.take();
                        }
                        physical_row = physical_row.saturating_add(1);
                    });
                    clear_record_buffer(&mut record);
                    large_guard.take();
                    if let Some((err_row, err)) = deferred_error {
                        record_error(err_row, err);
                        break 'worker;
                    }
                    if stopped || should_stop(physical_row) {
                        break 'worker;
                    }
                    if let Err(err) = finish_result {
                        record_error(physical_row, err.into());
                        break 'worker;
                    }
                } else if !record.is_empty() || physical_row != stripe.end_row {
                    chunk_tracker.take();
                    chunk_credit.take();
                    record_error(physical_row, QuarryError::SourceChanged);
                    break 'worker;
                }

                let final_credit = chunk_credit.take();
                let final_tracker = chunk_tracker.take();
                if !emit_chunk_with_credit(
                    true,
                    final_credit,
                    final_tracker,
                    &mut chunk_bytes,
                    &mut relative_ends,
                    &mut chunk_replacements,
                    &mut chunk_rows,
                    &mut seq,
                    scanner.maximum_width(),
                ) {
                    break 'worker;
                }
            }

            clear_record_buffer(&mut record);
            large_guard.take();
        });
        if !spawned {
            break;
        }
    }
    drop(tx);

    let mut output_index = StructuralIndex::new(IndexConfig::default())?;
    let mut pending: BTreeMap<(usize, u64), ReplaceOutputChunk> = BTreeMap::new();
    let mut current_stripe = 0_usize;
    let mut expected_seq = 0_u64;
    let mut total_replacements = 0_u64;
    let mut total_physical_rows = 0_u64;
    let mut overall_max_width = 0_usize;
    let mut writer_error: Option<QuarryError> = None;

    let mut commit_chunk = |chunk: ReplaceOutputChunk,
                            output: &mut ExportTarget,
                            output_index: &mut StructuralIndex|
     -> Result<bool, QuarryError> {
        let base_offset = output.bytes_written();
        if !chunk.bytes.is_empty() {
            output.write_all(&chunk.bytes)?;
        }
        for &rel_end in &chunk.relative_record_ends {
            output_index.record_boundary(base_offset.saturating_add(rel_end));
        }
        total_replacements = total_replacements.saturating_add(chunk.replacements);
        total_physical_rows = total_physical_rows.saturating_add(chunk.physical_rows);
        overall_max_width = overall_max_width.max(chunk.max_width);
        let written = output.bytes_written();
        shared.bytes_written.store(written, Ordering::Release);
        let is_last = chunk.is_last;
        let was_head = chunk.is_head_credit;
        drop(chunk);
        if was_head {
            head_flow.release();
        }
        Ok(is_last)
    };

    while current_stripe < stripes.len() {
        if shared.cancel_requested.load(Ordering::Acquire) {
            stop_flag.store(true, Ordering::Release);
            head_flow.notify_all();
            break;
        }
        let msg = match rx.recv_timeout(std::time::Duration::from_millis(5)) {
            Ok(msg) => msg,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            StripeMessage::Failed => {
                // Keep draining incoming chunks without writing to disk so workers evaluating
                // earlier stripes can finish cleanly and report the exact earliest error.
                for (_, chunk) in std::mem::take(&mut pending) {
                    if chunk.is_head_credit {
                        head_flow.release();
                    }
                }
                loop {
                    if shared.cancel_requested.load(Ordering::Acquire) {
                        stop_flag.store(true, Ordering::Release);
                        head_flow.notify_all();
                        break;
                    }
                    match rx.recv_timeout(std::time::Duration::from_millis(5)) {
                        Ok(StripeMessage::Chunk(chunk)) => {
                            if chunk.is_head_credit {
                                head_flow.release();
                            }
                        }
                        Ok(StripeMessage::Failed) => {}
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                break;
            }
            StripeMessage::Chunk(chunk) => {
                if earliest_error_row.load(Ordering::Acquire) != u64::MAX {
                    if chunk.is_head_credit {
                        head_flow.release();
                    }
                    pending.clear();
                    continue;
                }
                if chunk.stripe_id == current_stripe && chunk.seq == expected_seq {
                    match commit_chunk(chunk, output, &mut output_index) {
                        Ok(is_last) => {
                            if is_last {
                                current_stripe += 1;
                                expected_seq = 0;
                                head_stripe_id.store(current_stripe, Ordering::Release);
                            } else {
                                expected_seq += 1;
                            }
                        }
                        Err(err) => {
                            writer_error = Some(err);
                            stop_flag.store(true, Ordering::Release);
                            head_flow.notify_all();
                            break;
                        }
                    }
                    while let Some(next_chunk) = pending.remove(&(current_stripe, expected_seq)) {
                        match commit_chunk(next_chunk, output, &mut output_index) {
                            Ok(is_last) => {
                                if is_last {
                                    current_stripe += 1;
                                    expected_seq = 0;
                                    head_stripe_id.store(current_stripe, Ordering::Release);
                                } else {
                                    expected_seq += 1;
                                }
                            }
                            Err(err) => {
                                writer_error = Some(err);
                                stop_flag.store(true, Ordering::Release);
                                head_flow.notify_all();
                                break;
                            }
                        }
                    }
                    if writer_error.is_some() {
                        break;
                    }
                } else {
                    pending.insert((chunk.stripe_id, chunk.seq), chunk);
                }
            }
        }
    }

    // Release any remaining pending buffers before draining workers so no worker stays parked.
    for (_, chunk) in std::mem::take(&mut pending) {
        if chunk.is_head_credit {
            head_flow.release();
        }
    }
    drop(rx);
    head_flow.notify_all();
    executor.drain()?;

    if let Some(err) = writer_error {
        return Err(err);
    }
    if shared.cancel_requested.load(Ordering::Acquire) {
        return Ok(None);
    }
    if let Some((_, err)) = earliest_error.lock().unwrap().take() {
        return Err(err);
    }
    if current_stripe != stripes.len() {
        return Err(QuarryError::SourceChanged);
    }
    if has_header && total_physical_rows == 0 {
        return Err(QuarryError::InvalidOption(
            "source does not contain a header row",
        ));
    }
    if edits
        .cells
        .last_key_value()
        .is_some_and(|((row, _), _)| *row >= total_physical_rows)
    {
        return Err(QuarryError::InvalidOption("cell edit row is out of range"));
    }

    let bytes_written = output.bytes_written();
    output_index.finish_output(bytes_written, overall_max_width);
    shared.bytes_written.store(bytes_written, Ordering::Release);
    Ok(Some(ReplaceParallelResult {
        bytes_written,
        replacements: total_replacements,
        output_index,
        effective_workers: actual_workers,
    }))
}
