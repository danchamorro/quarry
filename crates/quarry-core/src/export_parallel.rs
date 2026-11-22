use std::collections::BTreeMap;
use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use memchr::memchr;
use quarry_delimited::{
    ByteEncodingClass, ParseError, ParseErrorKind, ScannerDialect, StructuralScanner,
    active_simd_backend,
};

use super::{
    ExportConfig, ExportTarget, FilteredEdits, ScanOutcome, SharedState, UTF8_BOM, process_record,
    publish_progress, source_starts_with_bom,
};
use crate::Encoding;
use crate::QuarryError;
use crate::filter::{
    FilterEdits, FilterQuery, PredicateMatcher, compile_matchers, predicate_groups,
    record_matches_fast,
};
use crate::pool::{BudgetGuard, ByteBudget, JobExecutor, JobPriority, engine_pool};
use crate::positional_io::{advise_readahead, read_at_exact_or_eof};

pub(super) const PARALLEL_EXPORT_MIN_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const PARALLEL_EXPORT_CHUNK_BYTES: usize = 2 * 1024 * 1024;
pub(super) const DEFAULT_EXPORT_REORDER_BUDGET_BYTES: usize = 64 * 1024 * 1024;
const ALIGNMENT_PROBE_BYTES: usize = 64 * 1024;
const MAX_ALIGNMENT_SEARCH_BYTES: u64 = 256 * 1024;
const WORKER_CANCEL_SLICE_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub(super) struct ExportParallelTuning {
    pub(super) requested_threads: usize,
    pub(super) min_parallel_bytes: u64,
    pub(super) chunk_bytes: usize,
    pub(super) max_alignment_search: u64,
    pub(super) reorder_budget_bytes: usize,
    #[cfg(test)]
    pub(super) delay_chunk_zero: Option<Duration>,
    #[cfg(test)]
    pub(super) on_reorder_wait: Option<Arc<AtomicBool>>,
}

impl ExportParallelTuning {
    pub(super) fn for_production(requested_threads: usize) -> Self {
        Self {
            requested_threads,
            min_parallel_bytes: PARALLEL_EXPORT_MIN_BYTES,
            chunk_bytes: PARALLEL_EXPORT_CHUNK_BYTES,
            max_alignment_search: MAX_ALIGNMENT_SEARCH_BYTES,
            reorder_budget_bytes: DEFAULT_EXPORT_REORDER_BUDGET_BYTES,
            #[cfg(test)]
            delay_chunk_zero: None,
            #[cfg(test)]
            on_reorder_wait: None,
        }
    }
}

struct ExportRangeOutcome {
    output_bytes: Vec<u8>,
    rows_written: u64,
    rows_scanned: u64,
    first_match_raw: Option<Vec<u8>>,
    end_in_quote: bool,
    open_record_start: Option<u64>,
    error: Option<QuarryError>,
}

struct ExportQuotedOutcome {
    first_record_end: Option<u64>,
    first_error: Option<QuarryError>,
    tail: Option<ExportRangeOutcome>,
}

struct ExportChunkResult {
    id: u64,
    start: u64,
    end: u64,
    is_last: bool,
    field_start: ExportRangeOutcome,
    quoted: Option<ExportQuotedOutcome>,
    _reorder_guards: Vec<BudgetGuard>,
}

fn find_export_next_lf(
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
fn format_export_slice_from_field_start(
    id: u64,
    buf: &[u8],
    slice_start: u64,
    end: u64,
    is_last: bool,
    delimiter: u8,
    dialect: ScannerDialect,
    config: ExportConfig,
    byte_class: ByteEncodingClass,
    query: &FilterQuery,
    matchers: &[PredicateMatcher<'_>],
    groups: &[Vec<usize>],
    max_column: usize,
    reorder_budget: &Arc<ByteBudget>,
    next_merge_id: &AtomicU64,
    reorder_guards: &mut Vec<BudgetGuard>,
    reserved_bytes: &mut usize,
    stop_flag: &AtomicBool,
) -> Option<ExportRangeOutcome> {
    let mut scanner = match StructuralScanner::<false>::with_dialect_without_bom(
        dialect,
        slice_start,
        byte_class,
    ) {
        Ok(scanner) => scanner,
        Err(err) => {
            return Some(ExportRangeOutcome {
                output_bytes: Vec::new(),
                rows_written: 0,
                rows_scanned: 0,
                first_match_raw: None,
                end_in_quote: false,
                open_record_start: None,
                error: Some(QuarryError::Parse(err)),
            });
        }
    };

    let empty_edits: FilterEdits = Arc::default();
    let mut output_bytes: Vec<u8> = Vec::new();
    let mut first_match_raw: Option<Vec<u8>> = None;
    let mut rows_written = 0u64;
    let mut local_row = 0u64;
    let mut record_start = slice_start;
    let mut field_ranges = Vec::with_capacity((max_column + 1).min(64));
    let mut deferred_error: Option<QuarryError> = None;
    let mut cancelled_in_budget = false;

    let mut reserve_for_capacity = |needed_extra_cap: usize| -> bool {
        if needed_extra_cap == 0 {
            return true;
        }
        // Charge speculative output capacity before allocating so workers stall within
        // the 64 MiB reorder ceiling; head-of-line chunk bypasses to prevent deadlock.
        let Some(guard) = reorder_budget.acquire_or_bypass(needed_extra_cap, stop_flag, || {
            id == next_merge_id.load(Ordering::Acquire)
        }) else {
            return false;
        };
        *reserved_bytes = reserved_bytes.saturating_add(needed_extra_cap);
        reorder_guards.push(guard);
        true
    };

    let mut emit_record = |rec: &[u8], row: u64| -> Result<bool, QuarryError> {
        if rec.len() > config.max_record_bytes {
            return Err(QuarryError::RecordTooLarge {
                limit: config.max_record_bytes,
            });
        }
        let decoded = config
            .source_encoding
            .decode(rec, config.max_record_bytes)
            .map_err(|error| crate::record_encoding_error(row.saturating_add(1), error))?;
        let decoded_bytes = decoded.as_ref();
        if record_matches_fast(
            decoded_bytes,
            delimiter,
            row,
            query,
            matchers,
            groups,
            max_column,
            &empty_edits,
            Encoding::Utf8,
            &mut field_ranges,
        )? {
            let encoded = config
                .output_encoding
                .encode(decoded_bytes, config.max_record_bytes)?;
            if encoded.len() > config.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
            if first_match_raw.is_none() {
                if !reserve_for_capacity(decoded_bytes.len()) {
                    return Ok(false);
                }
                let mut first = Vec::with_capacity(decoded_bytes.len());
                first.extend_from_slice(decoded_bytes);
                let extra_cap = first.capacity().saturating_sub(decoded_bytes.len());
                if !reserve_for_capacity(extra_cap) {
                    return Ok(false);
                }
                first_match_raw = Some(first);
            } else {
                let enc = encoded.as_ref();
                let needed_len = output_bytes.len().saturating_add(enc.len());
                if needed_len > output_bytes.capacity() {
                    let prev_cap = output_bytes.capacity();
                    let next_cap = prev_cap.saturating_mul(2).max(needed_len).max(4096);
                    let delta = next_cap.saturating_sub(prev_cap);
                    if !reserve_for_capacity(delta) {
                        return Ok(false);
                    }
                    output_bytes.reserve_exact(delta);
                    let actual_extra = output_bytes.capacity().saturating_sub(next_cap);
                    if !reserve_for_capacity(actual_extra) {
                        return Ok(false);
                    }
                }
                output_bytes.extend_from_slice(enc);
            }
            rows_written += 1;
        }
        Ok(true)
    };

    let mut offset = 0usize;
    while offset < buf.len() {
        if stop_flag.load(Ordering::Acquire) {
            return None;
        }
        let sub_end = (offset + WORKER_CANCEL_SLICE_BYTES).min(buf.len());
        let scan_res = scanner.scan_chunk(
            &buf[offset..sub_end],
            slice_start + offset as u64,
            |absolute_end| {
                if deferred_error.is_none() && !cancelled_in_budget {
                    let s = (record_start - slice_start) as usize;
                    let e = (absolute_end - slice_start) as usize;
                    // Use non-zero synthetic row (>0) so row-0 BOM stripping is never applied to non-initial chunks.
                    match emit_record(&buf[s..e], local_row + 1) {
                        Ok(true) => {}
                        Ok(false) => cancelled_in_budget = true,
                        Err(err) => deferred_error = Some(err),
                    }
                }
                local_row += 1;
                record_start = absolute_end;
            },
        );
        if cancelled_in_budget {
            return None;
        }
        if deferred_error.is_some() {
            break;
        }
        if let Err(err) = scan_res {
            deferred_error = Some(QuarryError::Parse(err));
            break;
        }
        offset = sub_end;
    }

    if deferred_error.is_none() && !cancelled_in_budget && is_last {
        match scanner.finish(end, |_| {}) {
            Ok(true) => {
                let s = (record_start - slice_start) as usize;
                match emit_record(&buf[s..], local_row + 1) {
                    Ok(true) => {}
                    Ok(false) => return None,
                    Err(err) => deferred_error = Some(err),
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
    Some(ExportRangeOutcome {
        output_bytes,
        rows_written,
        rows_scanned: local_row,
        first_match_raw,
        end_in_quote,
        open_record_start,
        error: deferred_error,
    })
}

#[allow(clippy::too_many_arguments)]
fn scan_export_chunk_worker(
    id: u64,
    start: u64,
    end: u64,
    is_last: bool,
    file: &File,
    delimiter: u8,
    dialect: ScannerDialect,
    config: ExportConfig,
    byte_class: ByteEncodingClass,
    query: &FilterQuery,
    reorder_budget: &Arc<ByteBudget>,
    next_merge_id: &AtomicU64,
    stop_flag: &AtomicBool,
    #[cfg(test)] delay_chunk_zero: Option<Duration>,
) -> Option<Result<ExportChunkResult, QuarryError>> {
    #[cfg(test)]
    if id == 0
        && let Some(delay) = delay_chunk_zero
    {
        std::thread::sleep(delay);
    }

    if stop_flag.load(Ordering::Acquire) {
        return None;
    }

    let len = match usize::try_from(end - start) {
        Ok(len) => len,
        Err(_) => return Some(Err(QuarryError::InvalidOption("export chunk overflow"))),
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

    // Chunk 0 (start == 0) is always formatted sequentially via `process_record` on the writer
    // thread so row-0 BOM handling, header renaming, and edits match `scan_export` identically;
    // still scan chunk 0 here only for row count / end state if needed, OR let writer handle chunk 0 directly.
    if start == 0 {
        let reorder_guard = reorder_budget
            .acquire_or_bypass(1, stop_flag, || id == next_merge_id.load(Ordering::Acquire))?;
        return Some(Ok(ExportChunkResult {
            id,
            start,
            end,
            is_last,
            field_start: ExportRangeOutcome {
                output_bytes: Vec::new(),
                rows_written: 0,
                rows_scanned: 0,
                first_match_raw: None,
                end_in_quote: false,
                open_record_start: None,
                error: None,
            },
            quoted: None,
            _reorder_guards: vec![reorder_guard],
        }));
    }

    let mut reorder_guards = Vec::new();
    let mut reserved_bytes = 0usize;

    let field_start = format_export_slice_from_field_start(
        id,
        &buf,
        start,
        end,
        is_last,
        delimiter,
        dialect,
        config,
        byte_class,
        query,
        &matchers,
        &groups,
        max_column,
        reorder_budget,
        next_merge_id,
        &mut reorder_guards,
        &mut reserved_bytes,
        stop_flag,
    )?;

    let quoted = if dialect.quote.is_some_and(|q| memchr(q, &buf).is_some()) {
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
            let step_end = (pos + 4096).min(buf.len());
            if let Err(err) = q_scanner.scan_chunk(&buf[pos..step_end], start + pos as u64, |re| {
                if first_end.is_none() {
                    first_end = Some(re);
                }
            }) {
                // Only record errors that occur before the incoming record closes; errors after
                // `first_end` belong to the tail and are reported after validating the stitched row.
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
            Some(format_export_slice_from_field_start(
                id,
                tail_slice,
                fe,
                end,
                is_last,
                delimiter,
                dialect,
                config,
                byte_class,
                query,
                &matchers,
                &groups,
                max_column,
                reorder_budget,
                next_merge_id,
                &mut reorder_guards,
                &mut reserved_bytes,
                stop_flag,
            )?)
        } else {
            None
        };
        Some(ExportQuotedOutcome {
            first_record_end: first_end,
            first_error,
            tail,
        })
    } else {
        Some(ExportQuotedOutcome {
            first_record_end: None,
            first_error: is_last.then_some(QuarryError::Parse(ParseError {
                offset: end,
                kind: ParseErrorKind::UnterminatedQuote,
            })),
            tail: None,
        })
    };

    drop(buf);
    Some(Ok(ExportChunkResult {
        id,
        start,
        end,
        is_last,
        field_start,
        quoted,
        _reorder_guards: reorder_guards,
    }))
}

fn adjust_export_worker_error(error: QuarryError, base_row: u64) -> QuarryError {
    match error {
        QuarryError::RecordEncoding { record, cause } => QuarryError::RecordEncoding {
            // Worker used `local_row + 1` as synthetic row, so `record = (local_row + 1) + 1`.
            record: base_row.saturating_add(record.saturating_sub(1)),
            cause,
        },
        other => other,
    }
}

struct ExportMergeState {
    in_quote: bool,
    open_record_start: u64,
    rows_scanned: u64,
    rows_written: u64,
    bytes_written: u64,
    next_id: u64,
    next_merge_id: Arc<AtomicU64>,
    reorder_budget: Arc<ByteBudget>,
    pending: BTreeMap<u64, Result<ExportChunkResult, QuarryError>>,
}

impl ExportMergeState {
    fn new(next_merge_id: Arc<AtomicU64>, reorder_budget: Arc<ByteBudget>) -> Self {
        Self {
            in_quote: false,
            open_record_start: 0,
            rows_scanned: 0,
            rows_written: 0,
            bytes_written: 0,
            next_id: 0,
            next_merge_id,
            reorder_budget,
            pending: BTreeMap::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write_range_sequential(
        &mut self,
        file: &File,
        range_start: u64,
        range_end: u64,
        is_last: bool,
        delimiter: u8,
        data_start: u64,
        dialect: ScannerDialect,
        byte_class: ByteEncodingClass,
        query: &FilterQuery,
        edits: &FilteredEdits,
        matchers: &[PredicateMatcher<'_>],
        groups: &[Vec<usize>],
        config: ExportConfig,
        source_had_bom: bool,
        output: &mut ExportTarget,
    ) -> Result<(), QuarryError> {
        let len = usize::try_from(range_end - range_start)
            .map_err(|_| QuarryError::InvalidOption("export chunk overflow"))?;
        let mut buf = vec![0u8; len];
        if read_at_exact_or_eof(file, range_start, &mut buf)? != len {
            return Err(QuarryError::SourceChanged);
        }
        let mut scanner = if range_start == 0 {
            StructuralScanner::<false>::with_dialect(dialect, 0, byte_class)?
        } else {
            StructuralScanner::<false>::with_dialect_without_bom(dialect, range_start, byte_class)?
        };
        let mut rec_start = range_start;
        let mut deferred_error: Option<QuarryError> = None;
        let scan_res = scanner.scan_chunk(&buf, range_start, |absolute_end| {
            if deferred_error.is_none() {
                let s = (rec_start - range_start) as usize;
                let e = (absolute_end - range_start) as usize;
                if let Err(err) = process_record(
                    &buf[s..e],
                    self.rows_scanned,
                    data_start,
                    delimiter,
                    query,
                    edits,
                    matchers,
                    groups,
                    config.max_record_bytes,
                    source_had_bom,
                    output,
                    &mut self.rows_written,
                    &mut self.bytes_written,
                ) {
                    deferred_error = Some(err);
                }
            }
            self.rows_scanned += 1;
            rec_start = absolute_end;
        });
        if let Some(err) = deferred_error {
            return Err(err);
        }
        scan_res?;
        if is_last && scanner.finish(range_end, |_| {})? {
            let s = (rec_start - range_start) as usize;
            process_record(
                &buf[s..],
                self.rows_scanned,
                data_start,
                delimiter,
                query,
                edits,
                matchers,
                groups,
                config.max_record_bytes,
                source_had_bom,
                output,
                &mut self.rows_written,
                &mut self.bytes_written,
            )?;
            self.rows_scanned += 1;
            rec_start = range_end;
        }
        self.in_quote = scanner.is_in_quote();
        if self.in_quote {
            self.open_record_start = rec_start;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn write_preformatted_outcome(
        &mut self,
        outcome: ExportRangeOutcome,
        base_row: u64,
        source_had_bom: bool,
        config: ExportConfig,
        output: &mut ExportTarget,
    ) -> Result<(), QuarryError> {
        if let Some(err) = outcome.error {
            return Err(adjust_export_worker_error(err, base_row));
        }
        if let Some(first_raw) = outcome.first_match_raw {
            // Preserve the exact first-output-record BOM guard when this is the first written row.
            if self.bytes_written == 0 {
                output.begin_first_record_bom_guard(source_had_bom);
                output.write_all(&first_raw)?;
                let inserted = output.finish_first_record_bom_guard()?;
                if (first_raw.len() as u64).saturating_add(inserted)
                    > config.max_record_bytes as u64
                {
                    return Err(QuarryError::RecordTooLarge {
                        limit: config.max_record_bytes,
                    });
                }
            } else {
                output.write_all(&first_raw)?;
            }
        }
        if !outcome.output_bytes.is_empty() {
            // `output_bytes` are already encoded in `output_encoding`.
            output.write_raw_all(&outcome.output_bytes)?;
        }
        self.rows_written += outcome.rows_written;
        self.bytes_written = output.bytes_written();
        self.rows_scanned += outcome.rows_scanned;
        self.in_quote = outcome.end_in_quote;
        if let Some(open) = outcome.open_record_start {
            self.open_record_start = open;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_ready(
        &mut self,
        file: &File,
        delimiter: u8,
        data_start: u64,
        dialect: ScannerDialect,
        byte_class: ByteEncodingClass,
        query: &FilterQuery,
        edits: &FilteredEdits,
        matchers: &[PredicateMatcher<'_>],
        groups: &[Vec<usize>],
        config: ExportConfig,
        source_had_bom: bool,
        output: &mut ExportTarget,
        shared: &SharedState,
    ) -> Result<(), QuarryError> {
        while let Some(result) = self.pending.remove(&self.next_id) {
            self.next_id += 1;
            self.next_merge_id.store(self.next_id, Ordering::Release);
            self.reorder_budget.notify_all();
            let chunk = result?;

            if chunk.start == 0 {
                self.write_range_sequential(
                    file,
                    chunk.start,
                    chunk.end,
                    chunk.is_last,
                    delimiter,
                    data_start,
                    dialect,
                    byte_class,
                    query,
                    edits,
                    matchers,
                    groups,
                    config,
                    source_had_bom,
                    output,
                )?;
            } else if !self.in_quote {
                let base_row = self.rows_scanned;
                let has_edits_in_chunk = !edits.cells.is_empty()
                    && edits
                        .cells
                        .range(
                            (base_row, 0)
                                ..=(
                                    base_row.saturating_add(chunk.field_start.rows_scanned),
                                    usize::MAX,
                                ),
                        )
                        .next()
                        .is_some();
                if has_edits_in_chunk {
                    self.write_range_sequential(
                        file,
                        chunk.start,
                        chunk.end,
                        chunk.is_last,
                        delimiter,
                        data_start,
                        dialect,
                        byte_class,
                        query,
                        edits,
                        matchers,
                        groups,
                        config,
                        source_had_bom,
                        output,
                    )?;
                } else {
                    self.write_preformatted_outcome(
                        chunk.field_start,
                        base_row,
                        source_had_bom,
                        config,
                        output,
                    )?;
                }
            } else {
                let q = chunk
                    .quoted
                    .expect("non-initial LF-aligned export chunk has quoted outcome");
                if let Some(err) = q.first_error {
                    return Err(adjust_export_worker_error(err, self.rows_scanned));
                }
                if let Some(first_end) = q.first_record_end {
                    let stitched_len = first_end.saturating_sub(self.open_record_start);
                    if stitched_len > config.max_record_bytes as u64 {
                        return Err(QuarryError::RecordTooLarge {
                            limit: config.max_record_bytes,
                        });
                    }
                    let mut rec = vec![0u8; stitched_len as usize];
                    if read_at_exact_or_eof(file, self.open_record_start, &mut rec)? != rec.len() {
                        return Err(QuarryError::SourceChanged);
                    }
                    process_record(
                        &rec,
                        self.rows_scanned,
                        data_start,
                        delimiter,
                        query,
                        edits,
                        matchers,
                        groups,
                        config.max_record_bytes,
                        source_had_bom,
                        output,
                        &mut self.rows_written,
                        &mut self.bytes_written,
                    )?;
                    self.rows_scanned += 1;
                    self.in_quote = false;

                    if let Some(tail) = q.tail {
                        let tail_base = self.rows_scanned;
                        let has_edits_in_tail = !edits.cells.is_empty()
                            && edits
                                .cells
                                .range(
                                    (tail_base, 0)
                                        ..=(
                                            tail_base.saturating_add(tail.rows_scanned),
                                            usize::MAX,
                                        ),
                                )
                                .next()
                                .is_some();
                        if has_edits_in_tail {
                            self.write_range_sequential(
                                file,
                                first_end,
                                chunk.end,
                                chunk.is_last,
                                delimiter,
                                data_start,
                                dialect,
                                byte_class,
                                query,
                                edits,
                                matchers,
                                groups,
                                config,
                                source_had_bom,
                                output,
                            )?;
                        } else {
                            self.write_preformatted_outcome(
                                tail,
                                tail_base,
                                source_had_bom,
                                config,
                                output,
                            )?;
                        }
                    }
                }
            }

            publish_progress(
                shared,
                chunk.end,
                self.rows_scanned,
                self.rows_written,
                self.bytes_written,
            );
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_export_giant_line_sequential(
    file: &File,
    start: u64,
    min_stop: u64,
    file_size: u64,
    delimiter: u8,
    data_start: u64,
    dialect: ScannerDialect,
    byte_class: ByteEncodingClass,
    query: &FilterQuery,
    edits: &FilteredEdits,
    matchers: &[PredicateMatcher<'_>],
    groups: &[Vec<usize>],
    config: ExportConfig,
    source_had_bom: bool,
    read_chunk_bytes: usize,
    merge: &mut ExportMergeState,
    output: &mut ExportTarget,
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
    let mut rec_start = if merge.in_quote {
        merge.open_record_start
    } else {
        start
    };
    let mut buf = vec![0u8; read_chunk_bytes.clamp(64 * 1024, 1024 * 1024)];

    while pos < file_size {
        if shared.cancel_requested.load(Ordering::Acquire) {
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
                let rec_len = absolute_end.saturating_sub(rec_start);
                if rec_len > config.max_record_bytes as u64 {
                    deferred_error = Some(QuarryError::RecordTooLarge {
                        limit: config.max_record_bytes,
                    });
                } else if rec_start >= pos {
                    let s = (rec_start - pos) as usize;
                    let e = (absolute_end - pos) as usize;
                    if let Err(err) = process_record(
                        &buf[s..e],
                        merge.rows_scanned,
                        data_start,
                        delimiter,
                        query,
                        edits,
                        matchers,
                        groups,
                        config.max_record_bytes,
                        source_had_bom,
                        output,
                        &mut merge.rows_written,
                        &mut merge.bytes_written,
                    ) {
                        deferred_error = Some(err);
                    }
                } else {
                    let mut stitched = vec![0u8; rec_len as usize];
                    match read_at_exact_or_eof(file, rec_start, &mut stitched) {
                        Ok(n) if n == stitched.len() => {
                            if let Err(err) = process_record(
                                &stitched,
                                merge.rows_scanned,
                                data_start,
                                delimiter,
                                query,
                                edits,
                                matchers,
                                groups,
                                config.max_record_bytes,
                                source_had_bom,
                                output,
                                &mut merge.rows_written,
                                &mut merge.bytes_written,
                            ) {
                                deferred_error = Some(err);
                            }
                        }
                        Ok(_) => deferred_error = Some(QuarryError::SourceChanged),
                        Err(err) => deferred_error = Some(QuarryError::Io(err)),
                    }
                }
            }
            merge.rows_scanned += 1;
            rec_start = absolute_end;
        });
        if let Some(err) = deferred_error {
            return Err(err);
        }
        scan_res?;
        if next_pos.saturating_sub(rec_start) > config.max_record_bytes as u64 {
            return Err(QuarryError::RecordTooLarge {
                limit: config.max_record_bytes,
            });
        }
        if next_pos == file_size && scanner.finish(file_size, |_| {})? {
            let rec_len = file_size.saturating_sub(rec_start);
            if rec_len > config.max_record_bytes as u64 {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
            let mut stitched = vec![0u8; rec_len as usize];
            if read_at_exact_or_eof(file, rec_start, &mut stitched)? != stitched.len() {
                return Err(QuarryError::SourceChanged);
            }
            process_record(
                &stitched,
                merge.rows_scanned,
                data_start,
                delimiter,
                query,
                edits,
                matchers,
                groups,
                config.max_record_bytes,
                source_had_bom,
                output,
                &mut merge.rows_written,
                &mut merge.bytes_written,
            )?;
            merge.rows_scanned += 1;
            rec_start = file_size;
        }
        pos = next_pos;
        publish_progress(
            shared,
            pos,
            merge.rows_scanned,
            merge.rows_written,
            merge.bytes_written,
        );
        if hit_lf_after_min {
            break;
        }
    }

    merge.in_quote = scanner.is_in_quote();
    merge.open_record_start = rec_start;
    Ok(pos)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn scan_export_parallel(
    source: &mut File,
    output: &mut ExportTarget,
    delimiter: u8,
    data_start: u64,
    query: &FilterQuery,
    edits: &FilteredEdits,
    config: ExportConfig,
    byte_class: ByteEncodingClass,
    tuning: ExportParallelTuning,
    shared: &SharedState,
) -> Result<ScanOutcome, QuarryError> {
    let source_had_bom = source_starts_with_bom(source, config.source_encoding)?;
    let file_size = shared.total_bytes;
    let dialect = ScannerDialect::rfc4180(delimiter);
    dialect.validate(byte_class)?;
    let matchers = compile_matchers(query)?;
    let groups = predicate_groups(query);

    let chunk_size = tuning.chunk_bytes.max(64);
    let estimated_chunks = file_size.div_ceil(chunk_size as u64).max(1) as usize;
    let effective_workers = tuning.requested_threads.min(estimated_chunks).max(1);
    let pool = engine_pool();
    let executor = JobExecutor::new(pool, JobPriority::Foreground, effective_workers)?;
    let effective_workers = executor.effective_workers();

    let file = Arc::new(source.try_clone()?);
    #[cfg(test)]
    let reorder_budget = Arc::new(ByteBudget::with_wait_signal(
        tuning.reorder_budget_bytes.max(1),
        tuning.on_reorder_wait.clone(),
    ));
    #[cfg(not(test))]
    let reorder_budget = Arc::new(ByteBudget::new(tuning.reorder_budget_bytes.max(1)));
    let next_merge_id = Arc::new(AtomicU64::new(0));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Result<ExportChunkResult, QuarryError>>();
    let max_window = (effective_workers * 2).clamp(2, 16);
    let query_arc = Arc::new(query.clone());

    let mut merge = ExportMergeState::new(Arc::clone(&next_merge_id), Arc::clone(&reorder_budget));
    let mut cursor = 0u64;
    let mut next_chunk_id = 0u64;

    let run_result: Result<ScanOutcome, QuarryError> = (|| {
        while cursor < file_size {
            if shared.cancel_requested.load(Ordering::Acquire) {
                stop_flag.store(true, Ordering::Release);
                return Ok(ScanOutcome::Cancelled);
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
                    byte_class,
                    query,
                    edits,
                    &matchers,
                    &groups,
                    config,
                    source_had_bom,
                    output,
                    shared,
                ) {
                    stop_flag.store(true, Ordering::Release);
                    return Err(err);
                }
            }

            while (next_chunk_id - merge.next_id) as usize >= max_window {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    stop_flag.store(true, Ordering::Release);
                    return Ok(ScanOutcome::Cancelled);
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
                            byte_class,
                            query,
                            edits,
                            &matchers,
                            &groups,
                            config,
                            source_had_bom,
                            output,
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
                match find_export_next_lf(
                    &file,
                    nominal_end,
                    search_limit,
                    &shared.cancel_requested,
                )? {
                    Some(lf_pos) => lf_pos + 1,
                    None if shared.cancel_requested.load(Ordering::Acquire) => {
                        stop_flag.store(true, Ordering::Release);
                        return Ok(ScanOutcome::Cancelled);
                    }
                    None => {
                        while merge.next_id < next_chunk_id {
                            if shared.cancel_requested.load(Ordering::Acquire) {
                                stop_flag.store(true, Ordering::Release);
                                return Ok(ScanOutcome::Cancelled);
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
                                        byte_class,
                                        query,
                                        edits,
                                        &matchers,
                                        &groups,
                                        config,
                                        source_had_bom,
                                        output,
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
                        cursor = scan_export_giant_line_sequential(
                            &file,
                            cursor,
                            search_limit,
                            file_size,
                            delimiter,
                            data_start,
                            dialect,
                            byte_class,
                            query,
                            edits,
                            &matchers,
                            &groups,
                            config,
                            source_had_bom,
                            chunk_size,
                            &mut merge,
                            output,
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
                .map_err(|_| QuarryError::InvalidOption("export chunk overflow"))?;
            let Some(read_budget_guard) = pool
                .byte_budget()
                .acquire(chunk_len.max(1), &shared.cancel_requested)
            else {
                stop_flag.store(true, Ordering::Release);
                return Ok(ScanOutcome::Cancelled);
            };
            advise_readahead(&file, start, end - start);

            let worker_file = Arc::clone(&file);
            let worker_query = Arc::clone(&query_arc);
            let worker_reorder = Arc::clone(&reorder_budget);
            let worker_next_merge = Arc::clone(&next_merge_id);
            let worker_stop = Arc::clone(&stop_flag);
            let worker_tx = tx.clone();
            #[cfg(test)]
            let delay_zero = tuning.delay_chunk_zero;

            if !executor.spawn(&shared.cancel_requested, move || {
                let _read_guard = read_budget_guard;
                let res = scan_export_chunk_worker(
                    id,
                    start,
                    end,
                    is_last,
                    &worker_file,
                    delimiter,
                    dialect,
                    config,
                    byte_class,
                    &worker_query,
                    &worker_reorder,
                    &worker_next_merge,
                    &worker_stop,
                    #[cfg(test)]
                    delay_zero,
                );
                drop(_read_guard);
                if let Some(res) = res {
                    let _ = worker_tx.send(res);
                }
            }) {
                stop_flag.store(true, Ordering::Release);
                return Ok(ScanOutcome::Cancelled);
            }
        }

        while merge.next_id < next_chunk_id {
            if shared.cancel_requested.load(Ordering::Acquire) {
                stop_flag.store(true, Ordering::Release);
                return Ok(ScanOutcome::Cancelled);
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
                        byte_class,
                        query,
                        edits,
                        &matchers,
                        &groups,
                        config,
                        source_had_bom,
                        output,
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

        if edits
            .cells
            .keys()
            .next_back()
            .is_some_and(|(row, _)| *row >= merge.rows_scanned)
        {
            return Err(QuarryError::InvalidOption(
                "cell edit points beyond the source rows",
            ));
        }

        if merge.bytes_written == 0 && source_had_bom {
            output.write_all(UTF8_BOM)?;
            merge.bytes_written = output.bytes_written();
            publish_progress(
                shared,
                file_size,
                merge.rows_scanned,
                merge.rows_written,
                merge.bytes_written,
            );
        }

        Ok(ScanOutcome::Complete {
            rows_written: merge.rows_written,
            bytes_written: merge.bytes_written,
        })
    })();

    if run_result.is_err()
        || matches!(&run_result, Ok(ScanOutcome::Cancelled))
        || shared.cancel_requested.load(Ordering::Acquire)
    {
        stop_flag.store(true, Ordering::Release);
    }
    // Clear pending buffered results before draining so any stalled worker holding/waiting on
    // `reorder_budget` unblocks immediately.
    merge.pending.clear();
    drop(tx);
    let drain_res = executor.drain();
    let outcome = run_result?;
    drain_res?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::ExportParallelTuning;
    use crate::export::{FilterExportJob, FilterExportOutcome, FilteredEdits};
    use crate::{
        CaseSensitivity, FilterEdits, FilterOperator, FilterPredicate, FilterQuery, OpenOptions,
        QuarryError, Session,
    };

    #[test]
    fn parallel_filtered_export_matches_sequential_with_multiline_quotes_and_edits() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.csv");
        let seq_dest = dir.path().join("seq.csv");
        let par_dest = dir.path().join("par.csv");

        let mut data = Vec::new();
        data.extend_from_slice(b"\xEF\xBB\xBFid,score,note\n");
        for i in 1..=300_u32 {
            if i % 8 == 0 {
                data.extend_from_slice(
                    format!("{i},{},\"multi\nline\n\"\"quoted\"\"\n{i}\"\r\n", i % 60).as_bytes(),
                );
            } else {
                data.extend_from_slice(format!("{i},{},plain_{i}\n", i % 60).as_bytes());
            }
        }
        data.extend_from_slice(b"301,55,\"final\nmultiline\"");
        fs::write(&source, &data).unwrap();

        let session = Session::open(&source, OpenOptions::default()).unwrap();
        let query = FilterQuery {
            predicates: vec![FilterPredicate {
                column: 1,
                operator: FilterOperator::GreaterThan,
                value: b"40".to_vec(),
                upper_bound: None,
            }],
            case_sensitivity: CaseSensitivity::Sensitive,
        };
        let mut cell_map = BTreeMap::new();
        cell_map.insert((4_u64, 1usize), Arc::new(b"99".to_vec()));
        cell_map.insert((45_u64, 1usize), Arc::new(b"1".to_vec()));
        let cells: FilterEdits = Arc::new(cell_map);
        let headers = BTreeMap::from([(1usize, b"ScoreRenamed".to_vec())]);

        let seq_out = session
            .start_filtered_export_with_edits_and_threads(
                query.clone(),
                Arc::clone(&cells),
                headers.clone(),
                &seq_dest,
                Some(1),
            )
            .unwrap()
            .wait()
            .unwrap();
        let FilterExportOutcome::Complete(seq_summary) = seq_out else {
            panic!("expected sequential export complete");
        };

        let par_out = FilterExportJob::start_with_edits_and_tuning(
            session.path().to_path_buf(),
            session.file_size,
            session.dialect.delimiter,
            session.dialect.has_header,
            query,
            par_dest.clone(),
            session.export_config(),
            FilteredEdits { cells, headers },
            Some(session.source_stamp.clone()),
            ExportParallelTuning {
                requested_threads: 4,
                min_parallel_bytes: 64,
                chunk_bytes: 256,
                max_alignment_search: 256,
                reorder_budget_bytes: 64 * 1024,
                delay_chunk_zero: Some(Duration::from_millis(20)),
                on_reorder_wait: None,
            },
        )
        .unwrap()
        .wait()
        .unwrap();
        let FilterExportOutcome::Complete(par_summary) = par_out else {
            panic!("expected parallel export complete");
        };

        assert_eq!(par_summary.rows_written, seq_summary.rows_written);
        assert_eq!(par_summary.bytes_written, seq_summary.bytes_written);
        assert_eq!(fs::read(&par_dest).unwrap(), fs::read(&seq_dest).unwrap());
    }

    #[test]
    fn parallel_filtered_export_cancels_cleanly_under_reorder_backpressure() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("backpressure.csv");
        let dest = dir.path().join("out.csv");

        let mut data = Vec::with_capacity(200_000);
        data.extend_from_slice(b"id,payload\n");
        for i in 1..=2_000_u32 {
            data.extend_from_slice(
                format!("{i},match_payload_{i:06}_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n")
                    .as_bytes(),
            );
        }
        fs::write(&source, &data).unwrap();

        let session = Session::open(&source, OpenOptions::default()).unwrap();
        let query = FilterQuery::single(1, FilterOperator::Contains, b"match_payload".to_vec());

        // Tiny reorder budget (4 KiB) + slow chunk 0 forces later workers to stall on backpressure.
        let waited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let job = FilterExportJob::start_with_edits_and_tuning(
            session.path().to_path_buf(),
            session.file_size,
            session.dialect.delimiter,
            session.dialect.has_header,
            query,
            dest.clone(),
            session.export_config(),
            FilteredEdits::default(),
            Some(session.source_stamp.clone()),
            ExportParallelTuning {
                requested_threads: 4,
                min_parallel_bytes: 64,
                chunk_bytes: 4 * 1024,
                max_alignment_search: 1024,
                reorder_budget_bytes: 4 * 1024,
                delay_chunk_zero: Some(Duration::from_millis(500)),
                on_reorder_wait: Some(Arc::clone(&waited)),
            },
        )
        .unwrap();

        // Wait deterministically until a worker actually parks on the reorder budget.
        let wait_deadline = Instant::now() + Duration::from_secs(5);
        while !waited.load(std::sync::atomic::Ordering::Acquire) {
            assert!(
                Instant::now() < wait_deadline,
                "expected at least one worker to hit reorder backpressure"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let cancel_started = Instant::now();
        job.cancel();
        let outcome = job.wait().unwrap();
        let cancel_elapsed = cancel_started.elapsed();

        assert_eq!(outcome, FilterExportOutcome::Cancelled);
        assert!(
            cancel_elapsed < Duration::from_secs(1),
            "cancellation under backpressure stalled: {cancel_elapsed:?}"
        );
        assert!(
            !dest.exists(),
            "cancelled export must not publish destination"
        );
        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(
            entries.len(),
            1,
            "cancelled export must leave zero staged temporary files"
        );
    }

    #[test]
    fn parallel_filtered_export_preceding_parse_error_discards_staging() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("malformed.csv");
        let dest = dir.path().join("out.csv");

        let mut data = Vec::with_capacity(1_300_000);
        data.extend_from_slice(b"id,val\n");
        for i in 1..=25_000_u32 {
            data.extend_from_slice(
                format!("{i:06},alpha_padding_for_bootstrap_clearance\n").as_bytes(),
            );
        }
        // Malformed row past Session::open's 1 MiB bootstrap window (~1.15 MiB).
        data.extend_from_slice(b"025001,\"broken\"quote\n");
        for i in 25_002..=28_000_u32 {
            data.extend_from_slice(
                format!("{i:06},alpha_padding_for_bootstrap_clearance\n").as_bytes(),
            );
        }
        fs::write(&source, &data).unwrap();

        let session = Session::open(&source, OpenOptions::default()).unwrap();
        let query = FilterQuery::single(
            1,
            FilterOperator::Equals,
            b"alpha_padding_for_bootstrap_clearance".to_vec(),
        );

        let result = FilterExportJob::start_with_edits_and_tuning(
            session.path().to_path_buf(),
            session.file_size,
            session.dialect.delimiter,
            session.dialect.has_header,
            query,
            dest.clone(),
            session.export_config(),
            FilteredEdits::default(),
            Some(session.source_stamp.clone()),
            ExportParallelTuning {
                requested_threads: 4,
                min_parallel_bytes: 64,
                chunk_bytes: 64 * 1024,
                max_alignment_search: 4 * 1024,
                reorder_budget_bytes: 2 * 1024 * 1024,
                delay_chunk_zero: None,
                on_reorder_wait: None,
            },
        )
        .unwrap()
        .wait();

        assert!(
            matches!(result, Err(QuarryError::Parse(_))),
            "expected parse error, got {result:?}"
        );
        assert!(
            !dest.exists(),
            "failed export must not publish destination file"
        );
        let entries: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(
            entries.len(),
            1,
            "failed export must leave zero staged temporary files"
        );
    }

    #[test]
    fn parallel_filtered_export_never_deadlocks_when_later_chunks_fill_reorder_budget() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("head_of_line.csv");
        let seq_dest = dir.path().join("seq.csv");
        let par_dest = dir.path().join("par.csv");

        let mut data = Vec::with_capacity(200_000);
        data.extend_from_slice(b"id,payload\n");
        for i in 1..=1_500_u32 {
            data.extend_from_slice(
                format!("{i},match_payload_{i:06}_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n")
                    .as_bytes(),
            );
        }
        fs::write(&source, &data).unwrap();

        let session = Session::open(&source, OpenOptions::default()).unwrap();
        let query = FilterQuery::single(1, FilterOperator::Contains, b"match_payload".to_vec());

        let seq_out = session
            .start_filtered_export_with_edits_and_threads(
                query.clone(),
                Arc::default(),
                BTreeMap::new(),
                &seq_dest,
                Some(1),
            )
            .unwrap()
            .wait()
            .unwrap();
        let FilterExportOutcome::Complete(seq_summary) = seq_out else {
            panic!("expected sequential export complete");
        };

        // Later chunks exhaust the 4 KiB reorder budget while chunk 0 is delayed; head-of-line
        // bypass guarantees chunk 0 still acquires budget and drains without deadlocking.
        let par_out = FilterExportJob::start_with_edits_and_tuning(
            session.path().to_path_buf(),
            session.file_size,
            session.dialect.delimiter,
            session.dialect.has_header,
            query,
            par_dest.clone(),
            session.export_config(),
            FilteredEdits::default(),
            Some(session.source_stamp.clone()),
            ExportParallelTuning {
                requested_threads: 4,
                min_parallel_bytes: 64,
                chunk_bytes: 4 * 1024,
                max_alignment_search: 1024,
                reorder_budget_bytes: 4 * 1024,
                delay_chunk_zero: Some(Duration::from_millis(60)),
                on_reorder_wait: None,
            },
        )
        .unwrap()
        .wait()
        .unwrap();
        let FilterExportOutcome::Complete(par_summary) = par_out else {
            panic!("expected parallel export complete");
        };

        assert_eq!(par_summary.rows_written, seq_summary.rows_written);
        assert_eq!(par_summary.bytes_written, seq_summary.bytes_written);
        assert_eq!(fs::read(&par_dest).unwrap(), fs::read(&seq_dest).unwrap());
    }
}
