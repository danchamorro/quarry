//! Bounded section documents (`Session::open_section`), checked `u64` byte-window math,
//! outward record-boundary snapping, trust rules (`BoundaryTrust`), and pausable/cancellable
//! background boundary verification (`VerifyBoundaryJob`).

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use memchr::{memchr, memrchr};

use crate::encoding::EncodedScanner;
use crate::export::source_matches_stamp;
use crate::positional_io::{advise_readahead, read_at_exact_or_eof};
use crate::source_stamp::open_regular_file;
use crate::{
    Checkpoint, DEFAULT_MAX_RECORD_BYTES, Encoding, OpenOptions, QuarryError, Row, SourceStamp,
    StructuralIndex,
};

/// Hard cap (`1 MiB`) on backward/forward alignment probing during speculative or quote-free
/// boundary snapping so `Session::open_section` always completes in bounded time and memory.
pub const MAX_SPECULATIVE_SEARCH_BYTES: usize = 1024 * 1024;

/// Maximum distance (`16 MiB`) that `Session::open_section` will synchronously scan from a
/// validated checkpoint (or byte 0) to establish `BoundaryTrust::Verified` immediately.
/// Larger spans open immediately with bounded alignment and offer async `VerifyBoundaryJob`.
const MAX_SYNC_CHECKPOINT_SCAN_BYTES: u64 = 16 * 1024 * 1024;

/// Validation probe window (`64 KiB`) scanned forward from each candidate LF during two-state
/// speculative alignment.
const SPECULATIVE_VALIDATE_BYTES: usize = 64 * 1024;

/// Exact binary byte unit used for unit-aware `u64` section inputs and formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ByteUnit {
    Bytes,
    KiB,
    MiB,
    GiB,
    TiB,
}

impl ByteUnit {
    pub const ALL: [Self; 5] = [Self::Bytes, Self::KiB, Self::MiB, Self::GiB, Self::TiB];

    pub const fn factor(self) -> u64 {
        match self {
            Self::Bytes => 1,
            Self::KiB => 1024,
            Self::MiB => 1024 * 1024,
            Self::GiB => 1024 * 1024 * 1024,
            Self::TiB => 1024 * 1024 * 1024 * 1024,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Bytes => "Bytes",
            Self::KiB => "KiB",
            Self::MiB => "MiB",
            Self::GiB => "GiB",
            Self::TiB => "TiB",
        }
    }

    /// Pick a sensible default unit for displaying a file of `file_size` bytes.
    pub const fn default_for_size(file_size: u64) -> Self {
        if file_size >= Self::TiB.factor() {
            Self::GiB
        } else if file_size >= Self::GiB.factor() {
            Self::MiB
        } else if file_size >= Self::MiB.factor() {
            Self::KiB
        } else {
            Self::Bytes
        }
    }
}

/// Parse an exact non-negative integer value (allowing optional `,` or `_` digit separators)
/// scaled by `unit` using checked `u64` arithmetic. Never rounds through `f64`.
pub fn parse_exact_byte_input(input: &str, unit: ByteUnit) -> Result<u64, &'static str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a whole number of bytes or units.");
    }
    if trimmed.starts_with('-') {
        return Err("Byte offsets and sizes must be zero or positive.");
    }
    let cleaned: String = trimmed
        .chars()
        .filter(|&ch| ch != ',' && ch != '_')
        .collect();
    if cleaned.is_empty() || !cleaned.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Enter an exact whole integer (no decimals or letters).");
    }
    let value = cleaned
        .parse::<u64>()
        .map_err(|_| "Value exceeds the 64-bit byte range.")?;
    value
        .checked_mul(unit.factor())
        .ok_or("Byte value overflows 64-bit range.")
}

/// Format an exact byte count as a whole integer when divisible by `unit`, or exact bytes.
pub fn format_exact_bytes_in_unit(bytes: u64, unit: ByteUnit) -> (u64, ByteUnit) {
    let factor = unit.factor();
    if factor == 1 || bytes.is_multiple_of(factor) {
        (bytes / factor, unit)
    } else {
        (bytes, ByteUnit::Bytes)
    }
}

/// Half-open byte interval `[base, base + len)` with checked `u64` arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ByteWindow {
    pub base: u64,
    pub len: u64,
}

impl ByteWindow {
    /// Construct a checked `ByteWindow`, rejecting `base + len > u64::MAX`.
    pub const fn new(base: u64, len: u64) -> Option<Self> {
        if base.checked_add(len).is_some() {
            Some(Self { base, len })
        } else {
            None
        }
    }

    /// Construct a checked `ByteWindow` from half-open `[start, end)`, rejecting `end < start`.
    pub const fn from_range(start: u64, end: u64) -> Option<Self> {
        match end.checked_sub(start) {
            Some(len) => Some(Self { base: start, len }),
            None => None,
        }
    }

    /// Exclusive end offset `base + len`, checked against `u64` overflow.
    pub const fn end(self) -> Option<u64> {
        self.base.checked_add(self.len)
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Clamp `[base, base + len)` into `[0, file_size]` using checked `u64` arithmetic.
    pub const fn clamp_to_file(self, file_size: u64) -> Self {
        let base = if self.base > file_size {
            file_size
        } else {
            self.base
        };
        let max_len = file_size - base;
        let len = if self.len > max_len {
            max_len
        } else {
            self.len
        };
        Self { base, len }
    }

    /// Convert a coarse slider fraction `0.0..=1.0` (e.g. 1% VoiceOver step) to a `u64` offset
    /// within `0..=file_size`. Intended strictly for coarse slider drags; exact integer fields
    /// always keep canonical `u64` values without passing through `f64`.
    pub fn slider_fraction_to_offset(fraction: f64, file_size: u64) -> u64 {
        if !fraction.is_finite() || fraction <= 0.0 || file_size == 0 {
            return 0;
        }
        if fraction >= 1.0 {
            return file_size;
        }
        // Use 10_000 integer basis points (0.01% resolution) in u128 so very large `file_size`
        // (e.g. 20 TiB) never loses alignment to endpoints or overflows.
        let bps = (fraction * 10_000.0).round().clamp(0.0, 10_000.0) as u128;
        ((file_size as u128 * bps) / 10_000) as u64
    }

    pub fn offset_to_slider_fraction(offset: u64, file_size: u64) -> f64 {
        if file_size == 0 {
            return 0.0;
        }
        let clamped = offset.min(file_size);
        (clamped as f64 / file_size as f64).clamp(0.0, 1.0)
    }
}

/// Trust tier for a section's snapped record boundaries (`T11` / ADR 0014).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryTrust {
    /// Boundaries proved by scanning from a validated checkpoint/byte 0 under the same source
    /// generation, or by an explicit user-confirmed quote-free dialect (`quote = None`) with
    /// encoding-aligned record terminators.
    Verified,
    /// Bounded (`<= 1 MiB`) two-state speculative LF alignment; first record may be torn if a
    /// multiline quoted field crosses `snapped.base`.
    Speculative,
}

/// Underlying proof explaining how `SectionBounds` were established and whether global row
/// ordinals are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryProof {
    /// Scanned sequentially from byte 0 or a validated same-generation checkpoint.
    CountedScan {
        /// 0-based physical row ordinal of the first record inside `snapped`.
        first_global_row: u64,
    },
    /// User explicitly confirmed `quote = None` and snapped endpoints fell on encoding-aligned
    /// record terminators; global row ordinal remains unknown.
    QuoteFreeConfirmed,
    /// Two-state LF speculation within the 1 MiB cap; global row ordinal remains unknown.
    SpeculativeAlignment,
}

/// Requested and snapped byte intervals plus trust metadata for an open section document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionBounds {
    pub requested: ByteWindow,
    pub snapped: ByteWindow,
    pub trust: BoundaryTrust,
    pub proof: BoundaryProof,
}

impl SectionBounds {
    /// Returns the exact 0-based physical starting row ordinal in the parent file **only** when
    /// a counted checkpoint/index scan proved it. Returns `None` (*"global row unknown"*) for
    /// both `SpeculativeAlignment` and `QuoteFreeConfirmed`.
    pub const fn first_global_row(&self) -> Option<u64> {
        match self.proof {
            BoundaryProof::CountedScan { first_global_row } => Some(first_global_row),
            BoundaryProof::QuoteFreeConfirmed | BoundaryProof::SpeculativeAlignment => None,
        }
    }

    pub const fn was_snapped(&self) -> bool {
        self.requested.base != self.snapped.base || self.requested.len != self.snapped.len
    }

    pub const fn snapped_end(&self) -> u64 {
        match self.snapped.end() {
            Some(end) => end,
            None => u64::MAX,
        }
    }
}

/// Options governing `Session::open_section`.
#[derive(Debug, Clone)]
pub struct SectionOpenOptions {
    pub open: OpenOptions,
    pub encoding: Option<Encoding>,
    /// Optional validated index from the parent file (same `SourceStamp`, dialect, encoding).
    /// When a checkpoint covers `window.base` within synchronous scan bounds, Quarry scans from
    /// that checkpoint to establish `BoundaryTrust::Verified` with exact global row ordinals.
    pub parent_index: Option<StructuralIndex>,
    /// Explicit user confirmation that the file contains no quoted fields (`quote = None`).
    /// Never inferred automatically; arbitrary even UTF-16 offsets alone never establish trust.
    pub quote_free_confirmed: bool,
    /// Maximum backward/forward search window for alignment snapping (clamped to `<= 1 MiB`).
    pub max_speculative_search_bytes: usize,
}

impl Default for SectionOpenOptions {
    fn default() -> Self {
        Self {
            open: OpenOptions::default(),
            encoding: None,
            parent_index: None,
            quote_free_confirmed: false,
            max_speculative_search_bytes: MAX_SPECULATIVE_SEARCH_BYTES,
        }
    }
}

/// Internal section provenance stored on `Session` when opened via `Session::open_section`.
#[derive(Debug, Clone)]
pub(crate) struct SectionMetadata {
    pub(crate) bounds: SectionBounds,
    /// Header row resolved once from byte 0 when `dialect.has_header` is true.
    pub(crate) header_row: Option<Row>,
    /// Byte length of the file's leading BOM at byte 0 (`0`, `2`, or `3`).
    pub(crate) bom_len: u64,
    /// True when this session reads a private section-only working copy (offsets `[0, file_size)`)
    /// rather than reading `[bounds.snapped.base, bounds.snapped_end())` directly from the parent.
    pub(crate) backed_by_working_copy: bool,
    /// Stamp of the parent file generation from which a section working copy was derived.
    /// `None` for direct sections (whose own `Session::source_stamp` is the parent stamp).
    pub(crate) origin_parent_stamp: Option<SourceStamp>,
}

/// Live progress snapshot of a background `VerifyBoundaryJob`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyBoundaryProgress {
    pub bytes_scanned: u64,
    pub total_bytes: u64,
    pub rows_counted: u64,
    pub elapsed: Duration,
    pub estimated_remaining: Option<Duration>,
    pub paused: bool,
    pub cancelled: bool,
    pub done: bool,
}

/// Terminal outcome of a `VerifyBoundaryJob`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyBoundaryOutcome {
    Verified {
        bounds: SectionBounds,
        /// True when true record boundaries differ from the section's original snapped window.
        boundary_changed: bool,
    },
    Cancelled,
}

#[derive(Debug)]
struct VerifyPausedState {
    cursor: u64,
    rows_counted: u64,
    aligned_start: Option<u64>,
    first_global_row: Option<u64>,
    record_start: u64,
    scanner: Box<EncodedScanner<false>>,
}

struct VerifyLifecycle {
    worker_running: bool,
    pause_requested: bool,
    paused: bool,
    cancelled: bool,
    done: bool,
    paused_state: Option<VerifyPausedState>,
    outcome: Option<VerifyBoundaryOutcome>,
    terminal_error: Option<QuarryError>,
}

struct VerifyShared {
    bytes_scanned: AtomicU64,
    rows_counted: AtomicU64,
    total_bytes: u64,
    started: Instant,
    finished_nanos: AtomicU64,
    pause_requested: AtomicBool,
    paused: AtomicBool,
    cancelled: AtomicBool,
    done: AtomicBool,
    lifecycle: Mutex<VerifyLifecycle>,
    error: Mutex<Option<String>>,
}

#[derive(Clone)]
struct VerifyContext {
    path: PathBuf,
    source_stamp: SourceStamp,
    file_size: u64,
    delimiter: u8,
    encoding: Encoding,
    original_bounds: SectionBounds,
    start_checkpoint: Checkpoint,
}

struct VerifyWorkerCompletion<'a>(&'a VerifyShared);

impl Drop for VerifyWorkerCompletion<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lifecycle.lock().unwrap();
        state.worker_running = false;
        if state.paused && !state.cancelled && state.terminal_error.is_none() {
            return;
        }
        state.paused = false;
        state.pause_requested = false;
        state.paused_state = None;
        state.done = true;
        self.0.paused.store(false, Ordering::Release);
        self.0.pause_requested.store(false, Ordering::Release);
        let elapsed = self.0.started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        let _ = self.0.finished_nanos.compare_exchange(
            0,
            elapsed.max(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.0.done.store(true, Ordering::Release);
    }
}

/// Pausable, cancellable background worker that proves exact record boundaries and global row
/// ordinals for a bounded section document under `SourceStamp` guards.
pub struct VerifyBoundaryJob {
    shared: Arc<VerifyShared>,
    ctx: VerifyContext,
    handle: Option<JoinHandle<Result<(), QuarryError>>>,
}

impl VerifyBoundaryJob {
    pub(crate) fn start(
        path: PathBuf,
        source_stamp: SourceStamp,
        file_size: u64,
        delimiter: u8,
        encoding: Encoding,
        original_bounds: SectionBounds,
        parent_index: Option<&StructuralIndex>,
    ) -> Result<Self, QuarryError> {
        let file = open_regular_file(&path)?;
        if !source_matches_stamp(&file, &path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }

        let target_base = original_bounds.requested.base.min(file_size);
        let start_checkpoint = parent_index
            .filter(|idx| {
                idx.is_whole_file() && idx.matches_provenance(&source_stamp, delimiter, encoding)
            })
            .map(|idx| idx.checkpoint_at_or_before_offset(target_base))
            .filter(|cp| cp.offset <= target_base)
            .unwrap_or(Checkpoint { row: 0, offset: 0 });

        let target_end = original_bounds
            .requested
            .end()
            .unwrap_or(file_size)
            .max(original_bounds.snapped_end())
            .min(file_size);
        let total_bytes = target_end.saturating_sub(start_checkpoint.offset);

        let ctx = VerifyContext {
            path,
            source_stamp,
            file_size,
            delimiter,
            encoding,
            original_bounds,
            start_checkpoint,
        };
        let shared = Arc::new(VerifyShared {
            bytes_scanned: AtomicU64::new(0),
            rows_counted: AtomicU64::new(start_checkpoint.row),
            total_bytes,
            started: Instant::now(),
            finished_nanos: AtomicU64::new(0),
            pause_requested: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            done: AtomicBool::new(false),
            lifecycle: Mutex::new(VerifyLifecycle {
                worker_running: false,
                pause_requested: false,
                paused: false,
                cancelled: false,
                done: false,
                paused_state: None,
                outcome: None,
                terminal_error: None,
            }),
            error: Mutex::new(None),
        });
        let handle = Self::spawn_worker(&ctx, &shared, file, false)?;
        Ok(Self {
            shared,
            ctx,
            handle: Some(handle),
        })
    }

    fn spawn_worker(
        ctx: &VerifyContext,
        shared: &Arc<VerifyShared>,
        file: File,
        resuming: bool,
    ) -> Result<JoinHandle<Result<(), QuarryError>>, QuarryError> {
        let worker_shared = Arc::clone(shared);
        let ctx = ctx.clone();
        {
            let mut state = worker_shared.lifecycle.lock().unwrap();
            state.worker_running = true;
            if resuming {
                state.pause_requested = false;
                state.paused = false;
                worker_shared
                    .pause_requested
                    .store(false, Ordering::Release);
                worker_shared.paused.store(false, Ordering::Release);
            }
        }
        let spawn_result = thread::Builder::new()
            .name("quarry-verify-boundary".into())
            .spawn(move || {
                let _completion = VerifyWorkerCompletion(&worker_shared);
                let resume_from = if resuming {
                    worker_shared.lifecycle.lock().unwrap().paused_state.take()
                } else {
                    None
                };
                let result = run_verify_boundary(&file, &ctx, resume_from, &worker_shared);
                match result {
                    Ok(Some(paused_state)) => {
                        let mut state = worker_shared.lifecycle.lock().unwrap();
                        if !state.cancelled {
                            state.paused_state = Some(paused_state);
                            state.paused = true;
                            worker_shared.paused.store(true, Ordering::Release);
                        }
                        drop(file);
                        Ok(())
                    }
                    Ok(None) => Ok(()),
                    Err(error) => {
                        *worker_shared.error.lock().unwrap() = Some(error.to_string());
                        worker_shared.lifecycle.lock().unwrap().terminal_error =
                            Some(clone_verify_error(&error));
                        Err(error)
                    }
                }
            });
        match spawn_result {
            Ok(handle) => Ok(handle),
            Err(error) => {
                let mut state = shared.lifecycle.lock().unwrap();
                state.worker_running = false;
                if resuming && !state.cancelled && !state.done {
                    state.paused = true;
                    state.pause_requested = true;
                    shared.paused.store(true, Ordering::Release);
                    shared.pause_requested.store(true, Ordering::Release);
                }
                Err(error.into())
            }
        }
    }

    pub fn pause(&self) {
        let mut state = self.shared.lifecycle.lock().unwrap();
        if state.done || state.cancelled || state.paused || state.pause_requested {
            return;
        }
        state.pause_requested = true;
        self.shared.pause_requested.store(true, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Acquire) && !self.shared.done.load(Ordering::Acquire)
    }

    pub fn is_pause_requested(&self) -> bool {
        self.shared.pause_requested.load(Ordering::Acquire)
            && !self.shared.done.load(Ordering::Acquire)
    }

    pub fn resume(&mut self) -> Result<(), QuarryError> {
        {
            let state = self.shared.lifecycle.lock().unwrap();
            if state.done || state.cancelled {
                return Ok(());
            }
            if !state.pause_requested && !state.paused {
                return Ok(());
            }
        }
        if let Some(handle) = self.handle.take() {
            let join_res = handle.join().map_err(|_| QuarryError::WorkerPanicked)?;
            join_res?;
        }
        {
            let state = self.shared.lifecycle.lock().unwrap();
            if state.done || state.cancelled {
                return Ok(());
            }
        }
        let file = match open_regular_file(&self.ctx.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.mark_terminal_error(QuarryError::SourceChanged);
                return Err(QuarryError::SourceChanged);
            }
            Err(err) => {
                let err = QuarryError::Io(err);
                self.mark_terminal_error(clone_verify_error(&err));
                return Err(err);
            }
        };
        match source_matches_stamp(&file, &self.ctx.path, &self.ctx.source_stamp) {
            Ok(true) => {}
            Ok(false) => {
                self.mark_terminal_error(QuarryError::SourceChanged);
                return Err(QuarryError::SourceChanged);
            }
            Err(err) => {
                self.mark_terminal_error(clone_verify_error(&err));
                return Err(err);
            }
        }
        let handle = Self::spawn_worker(&self.ctx, &self.shared, file, true)?;
        self.handle = Some(handle);
        Ok(())
    }

    fn mark_terminal_error(&self, err: QuarryError) {
        *self.shared.error.lock().unwrap() = Some(err.to_string());
        let mut state = self.shared.lifecycle.lock().unwrap();
        state.terminal_error = Some(err);
        state.paused_state = None;
        state.paused = false;
        state.pause_requested = false;
        state.done = true;
        self.shared.paused.store(false, Ordering::Release);
        self.shared.pause_requested.store(false, Ordering::Release);
        let elapsed = self
            .shared
            .started
            .elapsed()
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        let _ = self.shared.finished_nanos.compare_exchange(
            0,
            elapsed.max(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.shared.done.store(true, Ordering::Release);
    }

    pub fn cancel(&self) {
        let mut state = self.shared.lifecycle.lock().unwrap();
        state.cancelled = true;
        state.pause_requested = false;
        self.shared.cancelled.store(true, Ordering::Release);
        self.shared.pause_requested.store(false, Ordering::Release);
        if !state.worker_running {
            state.paused_state = None;
            state.paused = false;
            state.done = true;
            state.outcome = Some(VerifyBoundaryOutcome::Cancelled);
            self.shared.paused.store(false, Ordering::Release);
            let elapsed = self
                .shared
                .started
                .elapsed()
                .as_nanos()
                .min(u64::MAX as u128) as u64;
            let _ = self.shared.finished_nanos.compare_exchange(
                0,
                elapsed.max(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.shared.done.store(true, Ordering::Release);
        }
    }

    pub fn progress(&self) -> VerifyBoundaryProgress {
        let done = self.shared.done.load(Ordering::Acquire);
        let paused = self.shared.paused.load(Ordering::Acquire);
        let cancelled = self.shared.cancelled.load(Ordering::Acquire);
        let bytes_scanned = self.shared.bytes_scanned.load(Ordering::Acquire);
        let rows_counted = self.shared.rows_counted.load(Ordering::Acquire);
        let total_bytes = self.shared.total_bytes;
        let finished_nanos = self.shared.finished_nanos.load(Ordering::Acquire);
        let elapsed = if finished_nanos == 0 {
            self.shared.started.elapsed()
        } else {
            Duration::from_nanos(finished_nanos)
        };
        let estimated_remaining =
            if done || paused || cancelled || bytes_scanned == 0 || bytes_scanned >= total_bytes {
                None
            } else {
                let elapsed_secs = elapsed.as_secs_f64();
                if elapsed_secs < 0.02 {
                    None
                } else {
                    let rate = bytes_scanned as f64 / elapsed_secs;
                    if rate > 0.0 && rate.is_finite() {
                        let rem_secs = (total_bytes - bytes_scanned) as f64 / rate;
                        if rem_secs.is_finite() && rem_secs >= 0.0 {
                            Some(Duration::from_secs_f64(rem_secs.min(86_400.0 * 365.0)))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
            };
        VerifyBoundaryProgress {
            bytes_scanned,
            total_bytes,
            rows_counted,
            elapsed,
            estimated_remaining,
            paused,
            cancelled,
            done,
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn wait(mut self) -> Result<VerifyBoundaryOutcome, QuarryError> {
        if let Some(handle) = self.handle.take() {
            let res = handle.join().map_err(|_| QuarryError::WorkerPanicked)?;
            res?;
        }
        let mut state = self.shared.lifecycle.lock().unwrap();
        if let Some(err) = state.terminal_error.take() {
            return Err(err);
        }
        if state.cancelled {
            return Ok(VerifyBoundaryOutcome::Cancelled);
        }
        Ok(state
            .outcome
            .take()
            .unwrap_or(VerifyBoundaryOutcome::Cancelled))
    }
}

impl Drop for VerifyBoundaryJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        } else {
            self.cancel();
        }
    }
}

fn clone_verify_error(error: &QuarryError) -> QuarryError {
    match error {
        QuarryError::SourceChanged => QuarryError::SourceChanged,
        QuarryError::Parse(err) => QuarryError::Parse(*err),
        QuarryError::Encoding(msg) => QuarryError::Encoding(msg.clone()),
        QuarryError::EncodingAt {
            encoding,
            offset,
            reason,
        } => QuarryError::EncodingAt {
            encoding: *encoding,
            offset: *offset,
            reason: reason.clone(),
        },
        QuarryError::RecordTooLarge { limit } => QuarryError::RecordTooLarge { limit: *limit },
        QuarryError::InvalidOption(msg) => QuarryError::InvalidOption(msg),
        QuarryError::WorkerPanicked => QuarryError::WorkerPanicked,
        QuarryError::SectionBoundaryNotFound {
            search_limit_bytes,
            reason,
        } => QuarryError::SectionBoundaryNotFound {
            search_limit_bytes: *search_limit_bytes,
            reason: reason.clone(),
        },
        other => QuarryError::Io(std::io::Error::other(other.to_string())),
    }
}

fn run_verify_boundary(
    file: &File,
    ctx: &VerifyContext,
    resume_from: Option<VerifyPausedState>,
    shared: &VerifyShared,
) -> Result<Option<VerifyPausedState>, QuarryError> {
    let req_start = ctx.original_bounds.requested.base.min(ctx.file_size);
    let req_end = ctx
        .original_bounds
        .requested
        .end()
        .unwrap_or(ctx.file_size)
        .min(ctx.file_size);

    let (
        mut cursor,
        mut rows_counted,
        mut aligned_start,
        mut first_global_row,
        mut record_start,
        mut scanner,
    ) = match resume_from {
        Some(state) => (
            state.cursor,
            state.rows_counted,
            state.aligned_start,
            state.first_global_row,
            state.record_start,
            state.scanner,
        ),
        None => (
            ctx.start_checkpoint.offset,
            ctx.start_checkpoint.row,
            None,
            None,
            ctx.start_checkpoint.offset,
            Box::new(EncodedScanner::<false>::at_offset(
                ctx.delimiter,
                ctx.start_checkpoint.offset,
                ctx.encoding,
            )?),
        ),
    };

    let chunk_size = ctx.encoding.chunk_size(256 * 1024);
    let mut chunk = vec![0u8; chunk_size];
    let mut aligned_end: Option<u64> = None;
    let scan_target_end = align_up_code_unit(req_end, ctx.file_size, ctx.encoding);

    advise_readahead(
        file,
        cursor,
        ctx.file_size.saturating_sub(cursor).min(4 * 1024 * 1024),
    );

    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            shared.lifecycle.lock().unwrap().outcome = Some(VerifyBoundaryOutcome::Cancelled);
            return Ok(None);
        }
        if shared.pause_requested.load(Ordering::Acquire) {
            return Ok(Some(VerifyPausedState {
                cursor,
                rows_counted,
                aligned_start,
                first_global_row,
                record_start,
                scanner,
            }));
        }

        if cursor >= ctx.file_size {
            scanner.finish(ctx.file_size, |record_end| {
                observe_record_for_bounds(
                    record_start,
                    record_end,
                    rows_counted,
                    req_start,
                    req_end,
                    &mut aligned_start,
                    &mut first_global_row,
                    &mut aligned_end,
                );
                rows_counted += 1;
                record_start = record_end;
            })?;
            break;
        }

        // Read up to `scan_target_end` first, then advance past `req_end` in bounded slices
        // truncated at the first encoding-aligned LF so malformed CSV after the closing record
        // boundary in the same 256 KiB chunk cannot invalidate a valid section's proof.
        let step_cap = if cursor < scan_target_end {
            (scan_target_end - cursor).min(chunk.len() as u64) as usize
        } else {
            (ctx.file_size - cursor).min(4096).min(chunk.len() as u64) as usize
        };
        let want = if ctx.encoding.is_utf16() && cursor + (step_cap as u64) < ctx.file_size {
            step_cap & !1
        } else {
            step_cap
        };
        if want == 0 {
            break;
        }
        let read = read_at_exact_or_eof(file, cursor, &mut chunk[..want])?;
        if read < want {
            return Err(QuarryError::SourceChanged);
        }

        let mut reached_target = false;
        if cursor >= scan_target_end {
            // Advance through already-read bytes LF-by-LF without re-reading overlapping ranges,
            // stopping immediately once the closing record covering `req_end` finishes.
            let mut pos = 0usize;
            while pos < read && !reached_target {
                let step_end =
                    find_first_lf_end(&chunk[pos..read], cursor + pos as u64, ctx.encoding)
                        .map(|abs| (abs - cursor) as usize)
                        .unwrap_or(read)
                        .min(read);
                scanner.scan_chunk(&chunk[pos..step_end], cursor + pos as u64, |record_end| {
                    observe_record_for_bounds(
                        record_start,
                        record_end,
                        rows_counted,
                        req_start,
                        req_end,
                        &mut aligned_start,
                        &mut first_global_row,
                        &mut aligned_end,
                    );
                    rows_counted += 1;
                    record_start = record_end;
                    if aligned_end.is_some() {
                        reached_target = true;
                    }
                })?;
                pos = step_end;
            }
            cursor += pos as u64;
        } else {
            scanner.scan_chunk(&chunk[..read], cursor, |record_end| {
                observe_record_for_bounds(
                    record_start,
                    record_end,
                    rows_counted,
                    req_start,
                    req_end,
                    &mut aligned_start,
                    &mut first_global_row,
                    &mut aligned_end,
                );
                rows_counted += 1;
                record_start = record_end;
                if aligned_end.is_some() {
                    reached_target = true;
                }
            })?;
            cursor += read as u64;
        }
        shared.bytes_scanned.store(
            cursor.saturating_sub(ctx.start_checkpoint.offset),
            Ordering::Release,
        );
        shared.rows_counted.store(rows_counted, Ordering::Release);

        if reached_target {
            break;
        }
    }

    if !source_matches_stamp(file, &ctx.path, &ctx.source_stamp)? {
        return Err(QuarryError::SourceChanged);
    }

    let final_start = aligned_start.unwrap_or(req_start);
    let final_row = first_global_row.unwrap_or(rows_counted);
    let final_end = aligned_end.unwrap_or(ctx.file_size).max(final_start);
    let snapped = ByteWindow::from_range(final_start, final_end)
        .ok_or(QuarryError::InvalidOption("invalid verified section range"))?;
    let verified = SectionBounds {
        requested: ctx.original_bounds.requested,
        snapped,
        trust: BoundaryTrust::Verified,
        proof: BoundaryProof::CountedScan {
            first_global_row: final_row,
        },
    };
    let boundary_changed = verified.snapped != ctx.original_bounds.snapped;
    shared.lifecycle.lock().unwrap().outcome = Some(VerifyBoundaryOutcome::Verified {
        bounds: verified,
        boundary_changed,
    });
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn observe_record_for_bounds(
    record_start: u64,
    record_end: u64,
    row_ordinal: u64,
    req_start: u64,
    req_end: u64,
    aligned_start: &mut Option<u64>,
    first_global_row: &mut Option<u64>,
    aligned_end: &mut Option<u64>,
) {
    // Outward snapping:
    // - A record [record_start, record_end) intersects [req_start, req_end) iff
    //   record_end > req_start && record_start < req_end.
    // - If req_start == record_start (already on a boundary), the previous record had
    //   record_end == req_start (NOT > req_start), so aligned_start preserves req_start!
    // - If req_end == record_end (already on a boundary), the next record has
    //   record_start == req_end (NOT < req_end), so aligned_end preserves req_end!
    if aligned_start.is_none() && record_end > req_start {
        *aligned_start = Some(record_start);
        *first_global_row = Some(row_ordinal);
    }
    if aligned_end.is_none() && record_end >= req_end {
        *aligned_end = Some(record_end);
    }
}

/// Snap `requested` outward to record boundaries and assign the appropriate `BoundaryTrust`
/// and `BoundaryProof` according to ADR 0014 (`T11`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn snap_section_window(
    file: &File,
    file_size: u64,
    delimiter: u8,
    encoding: Encoding,
    bom_len: u64,
    source_stamp: &SourceStamp,
    requested: ByteWindow,
    options: &SectionOpenOptions,
) -> Result<SectionBounds, QuarryError> {
    if requested.end().is_none() {
        return Err(QuarryError::InvalidOption(
            "section byte window overflows 64-bit range",
        ));
    }
    let clamped = requested.clamp_to_file(file_size);
    if file_size > 0 && clamped.len == 0 {
        return Err(QuarryError::InvalidOption(
            "section byte window must cover at least one byte within the file",
        ));
    }
    if file_size == 0 {
        return Ok(SectionBounds {
            requested: clamped,
            snapped: ByteWindow { base: 0, len: 0 },
            trust: BoundaryTrust::Verified,
            proof: BoundaryProof::CountedScan {
                first_global_row: 0,
            },
        });
    }

    let req_start = clamped.base;
    let req_end = clamped
        .end()
        .ok_or(QuarryError::InvalidOption("section end overflow"))?;

    // Tier 1: Validated whole-file checkpoint (or byte 0) within bounded synchronous scan distance.
    // Section indexes store section-local row numbers and may be speculative, so never adopt one
    // as a global parent index.
    let validated_cp = options
        .parent_index
        .as_ref()
        .filter(|idx| {
            idx.is_whole_file() && idx.matches_provenance(source_stamp, delimiter, encoding)
        })
        .map(|idx| idx.checkpoint_at_or_before_offset(req_start))
        .filter(|cp| cp.offset <= req_start);

    let sync_cp = match validated_cp {
        Some(cp) if req_end.saturating_sub(cp.offset) <= MAX_SYNC_CHECKPOINT_SCAN_BYTES => Some(cp),
        None if req_start == 0 && req_end <= MAX_SYNC_CHECKPOINT_SCAN_BYTES => {
            Some(Checkpoint { row: 0, offset: 0 })
        }
        _ => None,
    };

    let search_cap = options
        .max_speculative_search_bytes
        .clamp(1024, MAX_SPECULATIVE_SEARCH_BYTES);

    if let Some(cp) = sync_cp {
        return snap_from_validated_checkpoint(
            file, file_size, delimiter, encoding, clamped, cp, search_cap,
        );
    }

    // Tier 2: User-confirmed quote-free dialect (`quote = None`) with encoding-aligned terminators.
    // Note: global row ordinal remains unknown (`first_global_row() == None`).
    if options.quote_free_confirmed {
        return snap_quote_free_verified(file, file_size, encoding, bom_len, clamped, search_cap);
    }

    // Tier 3: Bounded `<= 1 MiB` two-state speculative LF alignment.
    // Even UTF-16 offsets alone never establish `BoundaryTrust::Verified`.
    snap_speculative_window(
        file, file_size, delimiter, encoding, bom_len, clamped, search_cap,
    )
}

fn snap_from_validated_checkpoint(
    file: &File,
    file_size: u64,
    delimiter: u8,
    encoding: Encoding,
    requested: ByteWindow,
    checkpoint: Checkpoint,
    search_cap: usize,
) -> Result<SectionBounds, QuarryError> {
    let req_start = requested.base;
    let req_end = requested.end().unwrap();
    let scan_target_end = align_up_code_unit(req_end, file_size, encoding);
    let max_scan_end = align_up_code_unit(
        scan_target_end.saturating_add(search_cap as u64),
        file_size,
        encoding,
    );
    let mut scanner = EncodedScanner::<false>::at_offset(delimiter, checkpoint.offset, encoding)?;
    let chunk_len = encoding.chunk_size(256 * 1024);
    let mut chunk = vec![0u8; chunk_len];
    let mut cursor = checkpoint.offset;
    let mut rows_counted = checkpoint.row;
    let mut record_start = checkpoint.offset;
    let mut aligned_start: Option<u64> = None;
    let mut first_global_row: Option<u64> = None;
    let mut aligned_end: Option<u64> = None;

    while cursor < max_scan_end && aligned_end.is_none() {
        // Read up to `scan_target_end` first, then advance past `req_end` in bounded slices
        // truncated at the first encoding-aligned LF so malformed CSV after the closing record
        // boundary in the same chunk never fails opening this valid section.
        let step_cap = if cursor < scan_target_end {
            (scan_target_end - cursor).min(chunk.len() as u64) as usize
        } else {
            (max_scan_end - cursor).min(4096).min(chunk.len() as u64) as usize
        };
        let want = if encoding.is_utf16() && cursor + (step_cap as u64) < file_size {
            step_cap & !1
        } else {
            step_cap
        };
        if want == 0 {
            break;
        }
        let read = read_at_exact_or_eof(file, cursor, &mut chunk[..want])?;
        if read < want {
            return Err(QuarryError::SourceChanged);
        }
        if cursor >= scan_target_end {
            // Advance through already-read bytes LF-by-LF without re-reading overlapping ranges,
            // stopping immediately once `aligned_end` is established.
            let mut pos = 0usize;
            while pos < read && aligned_end.is_none() {
                let step_end = find_first_lf_end(&chunk[pos..read], cursor + pos as u64, encoding)
                    .map(|abs| (abs - cursor) as usize)
                    .unwrap_or(read)
                    .min(read);
                scanner.scan_chunk(&chunk[pos..step_end], cursor + pos as u64, |record_end| {
                    observe_record_for_bounds(
                        record_start,
                        record_end,
                        rows_counted,
                        req_start,
                        req_end,
                        &mut aligned_start,
                        &mut first_global_row,
                        &mut aligned_end,
                    );
                    rows_counted += 1;
                    record_start = record_end;
                })?;
                pos = step_end;
            }
            cursor += pos as u64;
        } else {
            scanner.scan_chunk(&chunk[..read], cursor, |record_end| {
                observe_record_for_bounds(
                    record_start,
                    record_end,
                    rows_counted,
                    req_start,
                    req_end,
                    &mut aligned_start,
                    &mut first_global_row,
                    &mut aligned_end,
                );
                rows_counted += 1;
                record_start = record_end;
            })?;
            cursor += read as u64;
        }
    }

    if aligned_end.is_none() && cursor == file_size {
        scanner.finish(file_size, |record_end| {
            observe_record_for_bounds(
                record_start,
                record_end,
                rows_counted,
                req_start,
                req_end,
                &mut aligned_start,
                &mut first_global_row,
                &mut aligned_end,
            );
            rows_counted += 1;
            record_start = record_end;
        })?;
    }

    if aligned_end.is_none() && cursor < file_size {
        return Err(boundary_not_found(
            search_cap,
            "Could not find a record terminator after the requested section end within the bounded synchronous scan budget.",
        ));
    }

    let start = aligned_start.unwrap_or(req_start);
    let first_row = first_global_row.unwrap_or(rows_counted);
    let end = aligned_end.unwrap_or(file_size).max(start);
    let snapped = ByteWindow::from_range(start, end)
        .ok_or(QuarryError::InvalidOption("invalid snapped section bounds"))?;
    Ok(SectionBounds {
        requested,
        snapped,
        trust: BoundaryTrust::Verified,
        proof: BoundaryProof::CountedScan {
            first_global_row: first_row,
        },
    })
}

fn align_down_code_unit(offset: u64, encoding: Encoding) -> u64 {
    if encoding.is_utf16() {
        offset & !1
    } else {
        offset
    }
}

fn align_up_code_unit(offset: u64, file_size: u64, encoding: Encoding) -> u64 {
    if encoding.is_utf16() && !offset.is_multiple_of(2) {
        offset.saturating_add(1).min(file_size)
    } else {
        offset.min(file_size)
    }
}

fn lf_unit_len(encoding: Encoding) -> u64 {
    if encoding.is_utf16() { 2 } else { 1 }
}

fn is_lf_unit(bytes: &[u8], encoding: Encoding) -> bool {
    match encoding {
        Encoding::Utf16Le => bytes == [b'\n', 0],
        Encoding::Utf16Be => bytes == [0, b'\n'],
        Encoding::Utf8 | Encoding::Windows1252 | Encoding::Latin1 => bytes == *b"\n",
    }
}

/// Find the byte offset immediately AFTER the last encoding-aligned LF inside `buf`
/// (whose byte 0 sits at `buf_start` in the file), searching backward from the end of `buf`.
fn find_last_lf_end(buf: &[u8], buf_start: u64, encoding: Encoding) -> Option<u64> {
    if !encoding.is_utf16() {
        return memrchr(b'\n', buf).map(|idx| buf_start + idx as u64 + 1);
    }
    let step = 2usize;
    let even_len = buf.len() & !1;
    let mut pos = even_len;
    while pos >= step {
        pos -= step;
        if is_lf_unit(&buf[pos..pos + step], encoding) {
            return Some(buf_start + (pos + step) as u64);
        }
    }
    None
}

/// Find the byte offset immediately AFTER the first encoding-aligned LF inside `buf`
/// (whose byte 0 sits at `buf_start` in the file).
fn find_first_lf_end(buf: &[u8], buf_start: u64, encoding: Encoding) -> Option<u64> {
    if !encoding.is_utf16() {
        return memchr(b'\n', buf).map(|idx| buf_start + idx as u64 + 1);
    }
    let even_len = buf.len() & !1;
    let mut pos = 0usize;
    while pos + 2 <= even_len {
        if is_lf_unit(&buf[pos..pos + 2], encoding) {
            return Some(buf_start + (pos + 2) as u64);
        }
        pos += 2;
    }
    None
}

/// Collect all candidate start offsets (immediately after an encoding-aligned LF) in
/// `[probe_start, probe_end]` ordered from tightest outward (closest to `probe_end`) backwards.
fn collect_backward_lf_boundaries(
    buf: &[u8],
    buf_start: u64,
    encoding: Encoding,
    max_candidates: usize,
) -> Vec<u64> {
    let mut out = Vec::new();
    if !encoding.is_utf16() {
        let mut slice = buf;
        while out.len() < max_candidates {
            let Some(idx) = memrchr(b'\n', slice) else {
                break;
            };
            out.push(buf_start + idx as u64 + 1);
            slice = &slice[..idx];
        }
        return out;
    }
    let even_len = buf.len() & !1;
    let mut pos = even_len;
    while pos >= 2 && out.len() < max_candidates {
        pos -= 2;
        if is_lf_unit(&buf[pos..pos + 2], encoding) {
            out.push(buf_start + (pos + 2) as u64);
        }
    }
    out
}

fn contains_quote_unit(buf: &[u8], encoding: Encoding) -> bool {
    if !encoding.is_utf16() {
        return memchr(b'"', buf).is_some();
    }
    let even_len = buf.len() & !1;
    let needle = match encoding {
        Encoding::Utf16Le => [b'"', 0],
        Encoding::Utf16Be => [0, b'"'],
        _ => unreachable!(),
    };
    let mut pos = 0usize;
    while pos + 2 <= even_len {
        if buf[pos..pos + 2] == needle {
            return true;
        }
        pos += 2;
    }
    false
}

fn boundary_not_found(search_cap: usize, detail: impl Into<String>) -> QuarryError {
    QuarryError::SectionBoundaryNotFound {
        search_limit_bytes: search_cap,
        reason: format!(
            "{} Run full indexing to establish exact record boundaries across records longer than {} bytes.",
            detail.into(),
            search_cap,
        ),
    }
}

fn snap_quote_free_verified(
    file: &File,
    file_size: u64,
    encoding: Encoding,
    bom_len: u64,
    requested: ByteWindow,
    search_cap: usize,
) -> Result<SectionBounds, QuarryError> {
    let req_start = requested.base;
    let req_end = requested.end().unwrap();

    // Snap start outward.
    let aligned_start = if req_start <= bom_len {
        0
    } else {
        let end_probe = align_down_code_unit(req_start, encoding);
        let probe_start =
            align_down_code_unit(end_probe.saturating_sub(search_cap as u64), encoding);
        let probe_len = (end_probe - probe_start) as usize;
        let mut buf = vec![0u8; probe_len];
        let read = read_at_exact_or_eof(file, probe_start, &mut buf)?;
        if read < probe_len {
            return Err(QuarryError::SourceChanged);
        }
        if let Some(boundary) = find_last_lf_end(&buf, probe_start, encoding) {
            boundary
        } else if probe_start == 0 {
            0
        } else {
            return Err(boundary_not_found(
                search_cap,
                "No record boundary found before the requested section start within the 1 MiB search window.",
            ));
        }
    };

    // Snap end outward, preserving req_end if it is already at EOF or right after an LF.
    let aligned_end = if req_end >= file_size {
        file_size
    } else {
        let unit_len = lf_unit_len(encoding);
        let already_at_boundary = if req_end >= unit_len
            && (!encoding.is_utf16() || req_end.is_multiple_of(2))
        {
            let mut prev = [0u8; 2];
            let n = read_at_exact_or_eof(file, req_end - unit_len, &mut prev[..unit_len as usize])?;
            n == unit_len as usize && is_lf_unit(&prev[..unit_len as usize], encoding)
        } else {
            false
        };

        if already_at_boundary {
            req_end
        } else {
            let probe_start = align_down_code_unit(req_end, encoding);
            let probe_len = (file_size - probe_start).min(search_cap as u64) as usize;
            let probe_len = if encoding.is_utf16() {
                probe_len & !1
            } else {
                probe_len
            };
            let mut buf = vec![0u8; probe_len];
            let read = read_at_exact_or_eof(file, probe_start, &mut buf)?;
            if read < probe_len {
                return Err(QuarryError::SourceChanged);
            }
            if let Some(boundary) = find_first_lf_end(&buf, probe_start, encoding) {
                boundary
            } else if probe_start + probe_len as u64 >= file_size {
                file_size
            } else {
                return Err(boundary_not_found(
                    search_cap,
                    "No record boundary found after the requested section end within the 1 MiB search window.",
                ));
            }
        }
    };

    let aligned_end = align_up_code_unit(aligned_end, file_size, encoding).max(aligned_start);

    // Verify that the snapped section's boundary regions (or full section when <= 1 MiB) do not
    // actually contain RFC 4180 quote delimiters (`"`). Because Quarry's readers parse with `"`
    // quoting, granting `BoundaryTrust::Verified` when quotes are present would allow records to
    // cross LF boundaries unexpectedly.
    let check_range_quote_free = |start: u64, len: u64| -> Result<(), QuarryError> {
        let len = len.min(file_size.saturating_sub(start)) as usize;
        let len = if encoding.is_utf16() { len & !1 } else { len };
        if len == 0 {
            return Ok(());
        }
        let mut probe = vec![0u8; len];
        let read = read_at_exact_or_eof(file, start, &mut probe)?;
        if read < len {
            return Err(QuarryError::SourceChanged);
        }
        if contains_quote_unit(&probe, encoding) {
            return Err(QuarryError::InvalidOption(
                "Quoted field delimiter '\"' detected in section; uncheck 'No quoted fields' or run Verify Boundary.",
            ));
        }
        Ok(())
    };
    let section_len = aligned_end.saturating_sub(aligned_start);
    if section_len <= search_cap as u64 {
        check_range_quote_free(aligned_start, section_len)?;
    } else {
        let edge = (SPECULATIVE_VALIDATE_BYTES as u64).min(section_len);
        check_range_quote_free(aligned_start, edge)?;
        check_range_quote_free(aligned_end.saturating_sub(edge), edge)?;
    }

    let snapped = ByteWindow::from_range(aligned_start, aligned_end)
        .ok_or(QuarryError::InvalidOption("invalid snapped section bounds"))?;
    Ok(SectionBounds {
        requested,
        snapped,
        trust: BoundaryTrust::Verified,
        proof: BoundaryProof::QuoteFreeConfirmed,
    })
}

/// Validate a candidate interior record boundary by running `EncodedScanner` (`in_quote = false`)
/// over a bounded forward probe. When `stop_after_offset` is `Some(req_end)`, validation stops as
/// soon as a record closes at or past `req_end` so unrelated malformed CSV after the requested
/// section never rejects a valid start candidate.
/// Validate a speculative candidate offset by scanning forward up to `SPECULATIVE_VALIDATE_BYTES`,
/// stopping immediately once a complete record ends at or after `stop_after_offset` so unrelated
/// malformed CSV beyond the requested section never invalidates a valid boundary.
fn validate_speculative_candidate(
    file: &File,
    file_size: u64,
    candidate: u64,
    stop_after_offset: u64,
    delimiter: u8,
    encoding: Encoding,
) -> Result<bool, QuarryError> {
    let probe_len = (file_size - candidate).min(SPECULATIVE_VALIDATE_BYTES as u64) as usize;
    let probe_len = if encoding.is_utf16() {
        probe_len & !1
    } else {
        probe_len
    };
    if probe_len == 0 {
        return Ok(candidate == file_size);
    }
    let mut buf = vec![0u8; probe_len];
    let read = read_at_exact_or_eof(file, candidate, &mut buf)?;
    if read < probe_len {
        return Err(QuarryError::SourceChanged);
    }
    let mut scanner = EncodedScanner::<false>::at_offset(delimiter, candidate, encoding)?;
    let mut records = 0u64;
    let mut stopped_early = false;
    let scan_target = align_up_code_unit(stop_after_offset, file_size, encoding);
    let mut pos = 0usize;
    while pos < probe_len && !stopped_early {
        let abs = candidate + pos as u64;
        let step_end = if abs < scan_target {
            let clamped = (scan_target - abs).min((probe_len - pos) as u64) as usize;
            if encoding.is_utf16() && pos + clamped < probe_len {
                pos + (clamped & !1).max(2).min(probe_len - pos)
            } else {
                pos + clamped
            }
        } else {
            find_first_lf_end(&buf[pos..probe_len], abs, encoding)
                .map(|end_abs| (end_abs - candidate) as usize)
                .unwrap_or(probe_len)
                .min(probe_len)
        };
        if step_end <= pos {
            break;
        }
        if scanner
            .scan_chunk(&buf[pos..step_end], abs, |record_end| {
                records += 1;
                if record_end >= stop_after_offset {
                    stopped_early = true;
                }
            })
            .is_err()
        {
            return Ok(false);
        }
        pos = step_end;
    }
    if stopped_early {
        return Ok(true);
    }
    if candidate + (pos as u64) == file_size
        && scanner
            .finish(file_size, |_| {
                records += 1;
            })
            .is_err()
    {
        return Ok(false);
    }
    Ok(records > 0 || candidate + (pos as u64) < file_size)
}

/// Bounded fallback check for a large (`> 1 MiB`) speculative section when no backward LF anchor
/// exists within `search_cap` before `req_end` (e.g. a single record longer than `1 MiB` ending
/// exactly at `req_end`). Accepts `req_end` only when it is strictly before EOF, immediately
/// preceded by an encoding-aligned LF, and followed by valid CSV that completes at least one full
/// record within `search_cap` (always scanning at least `SPECULATIVE_VALIDATE_BYTES` or to EOF so
/// an interior LF of a multiline quoted field is rejected even when its closing quote lies beyond
/// 64 KiB or beyond `search_cap`).
fn is_validated_speculative_end_boundary(
    file: &File,
    file_size: u64,
    req_end: u64,
    delimiter: u8,
    encoding: Encoding,
    search_cap: usize,
) -> Result<bool, QuarryError> {
    let unit_len = lf_unit_len(encoding);
    if req_end < unit_len
        || req_end >= file_size
        || (encoding.is_utf16() && !req_end.is_multiple_of(2))
    {
        return Ok(false);
    }
    let mut prev = [0u8; 2];
    let n = read_at_exact_or_eof(file, req_end - unit_len, &mut prev[..unit_len as usize])?;
    if n != unit_len as usize || !is_lf_unit(&prev[..unit_len as usize], encoding) {
        return Ok(false);
    }

    let mut max_probe = (file_size - req_end).min(search_cap as u64);
    if encoding.is_utf16() && req_end + max_probe < file_size {
        max_probe &= !1;
    }
    if max_probe == 0 {
        return Ok(false);
    }
    let mut min_probe = max_probe.min(SPECULATIVE_VALIDATE_BYTES as u64);
    if encoding.is_utf16() && req_end + min_probe < file_size {
        min_probe &= !1;
    }

    let mut scanner = EncodedScanner::<false>::at_offset(delimiter, req_end, encoding)?;
    let mut buf = vec![0u8; encoding.chunk_size(SPECULATIVE_VALIDATE_BYTES)];
    let mut scanned = 0u64;
    let mut records = 0u64;
    while scanned < max_probe {
        if scanned >= min_probe && records > 0 {
            break;
        }
        let remaining = if scanned < min_probe {
            min_probe - scanned
        } else {
            max_probe - scanned
        };
        let mut want = remaining.min(buf.len() as u64) as usize;
        if encoding.is_utf16() && req_end + scanned + (want as u64) < file_size {
            want &= !1;
        }
        if want == 0 {
            break;
        }
        let read = read_at_exact_or_eof(file, req_end + scanned, &mut buf[..want])?;
        if read < want {
            return Err(QuarryError::SourceChanged);
        }
        if scanned >= min_probe {
            // Past `min_probe`, advance LF-by-LF only until the first following record finishes
            // (so a valid > 64 KiB next record succeeds without scanning unrelated trailing rows,
            // while a closing quote past 64 KiB triggers a parse error and fails closed).
            let mut pos = 0usize;
            while pos < read && records == 0 {
                let abs = req_end + scanned + pos as u64;
                let step_end = find_first_lf_end(&buf[pos..read], abs, encoding)
                    .map(|end_abs| (end_abs - (req_end + scanned)) as usize)
                    .unwrap_or(read)
                    .min(read);
                if step_end <= pos {
                    break;
                }
                if scanner
                    .scan_chunk(&buf[pos..step_end], abs, |_| {
                        records += 1;
                    })
                    .is_err()
                {
                    return Ok(false);
                }
                pos = step_end;
            }
            scanned += pos as u64;
        } else {
            if scanner
                .scan_chunk(&buf[..read], req_end + scanned, |_| {
                    records += 1;
                })
                .is_err()
            {
                return Ok(false);
            }
            scanned += read as u64;
        }
    }
    if req_end + scanned == file_size
        && scanner
            .finish(file_size, |_| {
                records += 1;
            })
            .is_err()
    {
        return Ok(false);
    }
    Ok(records > 0)
}

#[allow(clippy::too_many_arguments)]
fn scan_speculative_end_from_anchor(
    file: &File,
    file_size: u64,
    anchor: u64,
    req_end: u64,
    delimiter: u8,
    encoding: Encoding,
    search_cap: usize,
) -> Result<u64, QuarryError> {
    // Align the scan target outward to a code-unit boundary so odd UTF-16 end offsets never
    // collapse `step_cap` to zero before reaching the containing record terminator.
    let scan_target_end = align_up_code_unit(req_end, file_size, encoding);
    let mut scanner = EncodedScanner::<false>::at_offset(delimiter, anchor, encoding)?;
    let max_end = align_up_code_unit(
        scan_target_end.saturating_add(search_cap as u64),
        file_size,
        encoding,
    );
    let chunk_len = encoding.chunk_size(128 * 1024);
    let mut chunk = vec![0u8; chunk_len];
    let mut cursor = anchor;
    let mut found_end = None;
    while cursor < max_end && found_end.is_none() {
        // Read up to `scan_target_end` first, then advance in bounded 4 KiB slices LF-by-LF so we
        // stop immediately once the closing record covering `req_end` finishes without scanning
        // unrelated trailing bytes.
        let step_cap = if cursor < scan_target_end {
            (scan_target_end - cursor).min(chunk.len() as u64)
        } else {
            (max_end - cursor).min(4096).min(chunk.len() as u64)
        };
        let want = if encoding.is_utf16() && cursor + step_cap < file_size {
            (step_cap as usize) & !1
        } else {
            step_cap as usize
        };
        if want == 0 {
            break;
        }
        let read = read_at_exact_or_eof(file, cursor, &mut chunk[..want])?;
        if read < want {
            return Err(QuarryError::SourceChanged);
        }
        if cursor >= scan_target_end {
            let mut pos = 0usize;
            while pos < read && found_end.is_none() {
                let step_end = find_first_lf_end(&chunk[pos..read], cursor + pos as u64, encoding)
                    .map(|abs| (abs - cursor) as usize)
                    .unwrap_or(read)
                    .min(read);
                scanner.scan_chunk(&chunk[pos..step_end], cursor + pos as u64, |record_end| {
                    if found_end.is_none() && record_end >= req_end {
                        found_end = Some(record_end);
                    }
                })?;
                pos = step_end;
            }
            cursor += pos as u64;
        } else {
            scanner.scan_chunk(&chunk[..read], cursor, |record_end| {
                if found_end.is_none() && record_end >= req_end {
                    found_end = Some(record_end);
                }
            })?;
            cursor += read as u64;
        }
    }
    if found_end.is_none() && cursor == file_size {
        scanner.finish(file_size, |record_end| {
            if found_end.is_none() && record_end >= req_end {
                found_end = Some(record_end);
            }
        })?;
    }
    match found_end {
        Some(end) => Ok(end),
        None if cursor == file_size => Ok(file_size),
        None => Err(boundary_not_found(
            search_cap,
            "Could not find a record terminator after the requested section end within the 1 MiB alignment window.",
        )),
    }
}

fn snap_speculative_window(
    file: &File,
    file_size: u64,
    delimiter: u8,
    encoding: Encoding,
    bom_len: u64,
    requested: ByteWindow,
    search_cap: usize,
) -> Result<SectionBounds, QuarryError> {
    let req_start = requested.base;
    let req_end = requested.end().unwrap();

    // 1. Snap start outward within `<= 1 MiB`, validating candidate LF boundaries.
    let aligned_start = if req_start <= bom_len {
        0
    } else {
        let probe_end = align_down_code_unit(req_start, encoding);
        let probe_start =
            align_down_code_unit(probe_end.saturating_sub(search_cap as u64), encoding);
        let probe_len = (probe_end - probe_start) as usize;
        let mut buf = vec![0u8; probe_len];
        let read = read_at_exact_or_eof(file, probe_start, &mut buf)?;
        if read < probe_len {
            return Err(QuarryError::SourceChanged);
        }
        let candidates = collect_backward_lf_boundaries(&buf, probe_start, encoding, 16);
        let mut chosen = None;
        for candidate in candidates {
            if validate_speculative_candidate(
                file, file_size, candidate, req_end, delimiter, encoding,
            )? {
                chosen = Some(candidate);
                break;
            }
        }
        if let Some(boundary) = chosen {
            boundary
        } else if probe_start == 0 {
            0
        } else {
            return Err(boundary_not_found(
                search_cap,
                "Could not find a usable speculative record start within the 1 MiB alignment window.",
            ));
        }
    };

    // 2. Snap end outward within `<= 1 MiB` (preserving req_end when already on a clean boundary).
    let aligned_end = if req_end >= file_size {
        file_size
    } else if req_end.saturating_sub(aligned_start) <= search_cap as u64 {
        // When the requested window fits within `search_cap`, scan from `aligned_start` with exact
        // quote tracking so the end boundary is consistent with `aligned_start`.
        scan_speculative_end_from_anchor(
            file,
            file_size,
            aligned_start,
            req_end,
            delimiter,
            encoding,
            search_cap,
        )?
    } else {
        // Large section (> 1 MiB): anchor to a validated record start within `<= 1 MiB` before
        // `req_end`, then scan forward LF-by-LF stopping at the first `record_end >= req_end`.
        // This validates the record(s) ending at or crossing `req_end` without ever reading
        // unrelated malformed trailing data past the closing boundary.
        let unit_len = lf_unit_len(encoding);
        let probe_end = align_down_code_unit(req_end.saturating_sub(unit_len), encoding);
        let probe_start = align_down_code_unit(req_end.saturating_sub(search_cap as u64), encoding)
            .max(aligned_start);
        let tail_anchor = if probe_end > probe_start {
            let probe_len = (probe_end - probe_start) as usize;
            let mut buf = vec![0u8; probe_len];
            let read = read_at_exact_or_eof(file, probe_start, &mut buf)?;
            if read < probe_len {
                return Err(QuarryError::SourceChanged);
            }
            let candidates = collect_backward_lf_boundaries(&buf, probe_start, encoding, 16);
            let mut chosen = None;
            for candidate in candidates {
                if candidate >= req_end {
                    continue;
                }
                if validate_speculative_candidate(
                    file, file_size, candidate, req_end, delimiter, encoding,
                )? {
                    chosen = Some(candidate);
                    break;
                }
            }
            chosen.or((probe_start == aligned_start).then_some(aligned_start))
        } else {
            (probe_start == aligned_start).then_some(aligned_start)
        };
        if let Some(anchor) = tail_anchor {
            scan_speculative_end_from_anchor(
                file, file_size, anchor, req_end, delimiter, encoding, search_cap,
            )?
        } else if is_validated_speculative_end_boundary(
            file, file_size, req_end, delimiter, encoding, search_cap,
        )? {
            req_end
        } else {
            return Err(boundary_not_found(
                search_cap,
                "Could not verify a clean record end within the 1 MiB alignment window; use a counted scan or smaller section.",
            ));
        }
    };

    let snapped = ByteWindow::from_range(aligned_start, aligned_end.max(aligned_start))
        .ok_or(QuarryError::InvalidOption("invalid snapped section bounds"))?;
    Ok(SectionBounds {
        requested,
        snapped,
        trust: BoundaryTrust::Speculative,
        proof: BoundaryProof::SpeculativeAlignment,
    })
}

/// Read at most `wanted` records from byte 0 for section header resolution, advancing line-by-line
/// so malformed CSV after the `wanted`-th record in the same chunk is never parsed.
pub(crate) fn read_header_probe_rows(
    file: &File,
    file_size: u64,
    delimiter: u8,
    wanted: usize,
    limit: usize,
    encoding: Encoding,
) -> Result<(Vec<Row>, u64, bool), QuarryError> {
    if file_size == 0 || wanted == 0 {
        return Ok((Vec::new(), 0, file_size == 0));
    }
    let max_bytes = file_size.min(limit as u64);
    let mut scanner = EncodedScanner::<false>::at_offset(delimiter, 0, encoding)?;
    let chunk_cap = encoding.chunk_size(limit.clamp(4096, 64 * 1024));
    let mut chunk = vec![0u8; chunk_cap];
    let mut cursor = 0u64;
    let mut record = Vec::new();
    let mut record_start = 0u64;
    let mut rows = Vec::with_capacity(wanted);
    let mut physical_row = 0u64;

    while rows.len() < wanted && cursor < max_bytes {
        let remaining = max_bytes - cursor;
        let want = chunk
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let want = if encoding.is_utf16() && cursor + (want as u64) < file_size {
            want & !1
        } else {
            want
        };
        if want == 0 {
            break;
        }
        let read = read_at_exact_or_eof(file, cursor, &mut chunk[..want])?;
        if read == 0 {
            break;
        }
        let mut pos = 0usize;
        while pos < read && rows.len() < wanted {
            let step_end = find_first_lf_end(&chunk[pos..read], cursor + pos as u64, encoding)
                .map(|abs| (abs - cursor) as usize)
                .unwrap_or(read)
                .min(read);
            let mut seg_start = pos;
            let mut deferred_error = None;
            scanner.scan_chunk(&chunk[pos..step_end], cursor + pos as u64, |record_end| {
                let local_end = (record_end - cursor) as usize;
                if rows.len() < wanted && deferred_error.is_none() {
                    record.extend_from_slice(&chunk[seg_start..local_end]);
                    if record.len() > DEFAULT_MAX_RECORD_BYTES {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: DEFAULT_MAX_RECORD_BYTES,
                        });
                    } else {
                        match crate::parse_encoded_record_with_bom(
                            &record,
                            delimiter,
                            physical_row,
                            record_start == 0,
                            encoding,
                        ) {
                            Ok(fields) => rows.push(Row {
                                offset: record_start,
                                fields: fields.into_iter().map(|f| f.into_owned()).collect(),
                            }),
                            Err(err) => deferred_error = Some(err),
                        }
                    }
                    record.clear();
                }
                physical_row += 1;
                record_start = record_end;
                seg_start = local_end;
            })?;
            if let Some(err) = deferred_error {
                return Err(err);
            }
            if rows.len() < wanted && seg_start < step_end {
                record.extend_from_slice(&chunk[seg_start..step_end]);
                if record.len() > DEFAULT_MAX_RECORD_BYTES {
                    return Err(QuarryError::RecordTooLarge {
                        limit: DEFAULT_MAX_RECORD_BYTES,
                    });
                }
            }
            pos = step_end;
        }
        cursor += pos as u64;
    }

    if cursor == file_size && rows.len() < wanted && !record.is_empty() {
        let mut finished = false;
        scanner.finish(file_size, |_| {
            finished = true;
        })?;
        if finished {
            let fields = crate::parse_encoded_record_with_bom(
                &record,
                delimiter,
                physical_row,
                record_start == 0,
                encoding,
            )?
            .into_iter()
            .map(|f| f.into_owned())
            .collect();
            rows.push(Row {
                offset: record_start,
                fields,
            });
        }
    }

    Ok((rows, cursor, cursor >= file_size))
}

/// Materialize initial rows strictly from `[section_start, section_end)` using positional `pread`
/// (never seeking or scanning bytes before `section_start`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_section_initial_rows(
    file: &File,
    section_start: u64,
    section_end: u64,
    delimiter: u8,
    wanted: usize,
    limit: usize,
    encoding: Encoding,
    strip_bom_at_zero: bool,
) -> Result<(Vec<Row>, u64), QuarryError> {
    if section_start >= section_end || wanted == 0 {
        return Ok((Vec::new(), 0));
    }
    let max_bytes = (section_end - section_start).min(limit as u64);
    let mut scanner = EncodedScanner::<false>::at_offset(delimiter, section_start, encoding)?;
    let chunk_cap = encoding.chunk_size(limit.clamp(16 * 1024, 256 * 1024));
    let mut chunk = vec![0u8; chunk_cap];
    let mut cursor = section_start;
    let mut bytes_read = 0u64;
    let mut record = Vec::new();
    let mut record_start = section_start;
    let mut rows = Vec::with_capacity(wanted.min(256));
    let mut physical_row = 0u64;

    while rows.len() < wanted && cursor < section_end && bytes_read < max_bytes {
        let remaining = (section_end - cursor).min(max_bytes - bytes_read);
        let want = chunk
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let want = if encoding.is_utf16() { want & !1 } else { want };
        if want == 0 {
            break;
        }
        let read = read_at_exact_or_eof(file, cursor, &mut chunk[..want])?;
        if read == 0 {
            break;
        }
        let mut seg_start = 0usize;
        let mut deferred_error = None;
        scanner.scan_chunk(&chunk[..read], cursor, |record_end| {
            let local_end = (record_end - cursor) as usize;
            if rows.len() < wanted && deferred_error.is_none() {
                record.extend_from_slice(&chunk[seg_start..local_end]);
                if record.len() > DEFAULT_MAX_RECORD_BYTES {
                    deferred_error = Some(QuarryError::RecordTooLarge {
                        limit: DEFAULT_MAX_RECORD_BYTES,
                    });
                } else {
                    match crate::parse_encoded_record_with_bom(
                        &record,
                        delimiter,
                        physical_row,
                        strip_bom_at_zero && record_start == 0,
                        encoding,
                    ) {
                        Ok(fields) => {
                            rows.push(Row {
                                offset: record_start,
                                fields: fields.into_iter().map(|f| f.into_owned()).collect(),
                            });
                        }
                        Err(err) => deferred_error = Some(err),
                    }
                }
                record.clear();
            }
            physical_row += 1;
            record_start = record_end;
            seg_start = local_end;
        })?;
        if let Some(err) = deferred_error {
            return Err(err);
        }
        if rows.len() < wanted && seg_start < read {
            record.extend_from_slice(&chunk[seg_start..read]);
            if record.len() > DEFAULT_MAX_RECORD_BYTES {
                return Err(QuarryError::RecordTooLarge {
                    limit: DEFAULT_MAX_RECORD_BYTES,
                });
            }
        }
        cursor += read as u64;
        bytes_read += read as u64;
    }

    if cursor == section_end && rows.len() < wanted && !record.is_empty() {
        let mut finished = false;
        scanner.finish(section_end, |_| {
            finished = true;
        })?;
        if finished {
            let fields = crate::parse_encoded_record_with_bom(
                &record,
                delimiter,
                physical_row,
                strip_bom_at_zero && record_start == 0,
                encoding,
            )?
            .into_iter()
            .map(|f| f.into_owned())
            .collect();
            rows.push(Row {
                offset: record_start,
                fields,
            });
        }
    }

    Ok((rows, bytes_read))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};

    use super::*;
    use crate::{HeaderMode, IndexConfig, SaveAsOutcome, Session};

    #[test]
    fn checked_u64_range_math_and_exact_byte_parsing_stay_exact_above_2_to_53_and_20_tib() {
        // Strictly above f64's exact 53-bit integer ceiling (9_007_199_254_740_992).
        let over_2_53_a = (1u64 << 53) + 1;
        let over_2_53_b = (1u64 << 53) + 3;
        assert_ne!(over_2_53_a, over_2_53_b);
        assert_eq!(
            parse_exact_byte_input(&over_2_53_a.to_string(), ByteUnit::Bytes).unwrap(),
            over_2_53_a
        );
        assert_eq!(
            parse_exact_byte_input("9,007,199,254,740,995", ByteUnit::Bytes).unwrap(),
            over_2_53_b
        );
        let w = ByteWindow::from_range(over_2_53_a, over_2_53_b).unwrap();
        assert_eq!(w.base, over_2_53_a);
        assert_eq!(w.len, 2);
        assert_eq!(w.end(), Some(over_2_53_b));

        // 20 TiB exact checks.
        let twenty_tib = parse_exact_byte_input("20", ByteUnit::TiB).unwrap();
        assert_eq!(twenty_tib, 20u64 * 1024 * 1024 * 1024 * 1024);
        let eighteen_tib = parse_exact_byte_input("18", ByteUnit::TiB).unwrap();
        let slice = ByteWindow::new(eighteen_tib, 64 * 1024).unwrap();
        assert_eq!(slice.clamp_to_file(twenty_tib), slice);
        assert_eq!(slice.end(), Some(eighteen_tib + 64 * 1024));

        // Overflow rejection.
        assert!(ByteWindow::new(u64::MAX, 1).is_none());
        assert!(ByteWindow::from_range(10, 5).is_none());
        assert!(parse_exact_byte_input("18446744073709551616", ByteUnit::Bytes).is_err());
        assert!(parse_exact_byte_input("16777216", ByteUnit::TiB).is_err());
        assert!(parse_exact_byte_input("-5", ByteUnit::Bytes).is_err());
        assert!(parse_exact_byte_input("1.5", ByteUnit::GiB).is_err());

        // Coarse slider conversion bounds cleanly at 20 TiB.
        assert_eq!(ByteWindow::slider_fraction_to_offset(0.0, twenty_tib), 0);
        assert_eq!(
            ByteWindow::slider_fraction_to_offset(1.0, twenty_tib),
            twenty_tib
        );
        let half = ByteWindow::slider_fraction_to_offset(0.5, twenty_tib);
        assert_eq!(half, twenty_tib / 2);
    }

    #[test]
    fn outward_snapping_preserves_exact_boundaries_and_expands_interior_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.csv");
        // Byte layout:
        // row 0: "id,val\n"   [0..7)
        // row 1: "1,alpha\n"  [7..15)
        // row 2: "2,bravo\n"  [15..23)
        // row 3: "3,charlie\n"[23..33)
        fs::write(&path, b"id,val\n1,alpha\n2,bravo\n3,charlie\n").unwrap();

        let full = Session::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                delimiter: Some(b','),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let parent_index = full
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();

        // Exact boundaries [7..23) must be preserved without expanding into row 0 or row 3.
        let exact = Session::open_section(
            &path,
            ByteWindow::from_range(7, 23).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    delimiter: Some(b','),
                    ..OpenOptions::default()
                },
                parent_index: Some(parent_index.clone()),
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        let eb = exact.section_bounds().unwrap();
        assert_eq!(eb.snapped, ByteWindow::from_range(7, 23).unwrap());
        assert!(!eb.was_snapped());
        assert_eq!(eb.trust, BoundaryTrust::Verified);
        assert_eq!(eb.first_global_row(), Some(1));
        assert_eq!(exact.section_header_row().unwrap().fields[0], b"id");
        assert_eq!(exact.first_rows.len(), 2);
        assert_eq!(exact.first_rows[0].fields[1], b"alpha");
        assert_eq!(exact.first_rows[1].fields[1], b"bravo");

        // Interior endpoints [10..26) intersect row 1 [7..15), row 2 [15..23), and row 3 [23..33).
        let expanded = Session::open_section(
            &path,
            ByteWindow::from_range(10, 26).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    delimiter: Some(b','),
                    ..OpenOptions::default()
                },
                parent_index: Some(parent_index),
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        let xb = expanded.section_bounds().unwrap();
        assert_eq!(xb.requested, ByteWindow::from_range(10, 26).unwrap());
        assert_eq!(xb.snapped, ByteWindow::from_range(7, 33).unwrap());
        assert!(xb.was_snapped());
        assert_eq!(xb.trust, BoundaryTrust::Verified);
        assert_eq!(xb.first_global_row(), Some(1));
        assert_eq!(expanded.first_rows.len(), 3);
        assert_eq!(expanded.first_rows[2].fields[1], b"charlie");
    }

    #[test]
    fn trust_rules_distinguish_verified_quote_free_and_speculative_including_utf16() {
        let dir = tempfile::tempdir().unwrap();
        let utf16_path = dir.path().join("utf16.csv");
        // Write UTF-16 LE with BOM and enough rows (> 16 KiB) with no parent index.
        let mut text = String::from("id,name\n");
        for i in 1..=2000 {
            text.push_str(&format!("{i},row_{i:04}\n"));
        }
        let mut bytes = vec![0xFFu8, 0xFEu8];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(&utf16_path, &bytes).unwrap();
        // Force interior section without parent index (even UTF-16 offset alone proves nothing).
        let mid = ((bytes.len() as u64) / 2) & !1;
        let window = ByteWindow::new(mid, 256).unwrap();

        // 1. Without parent_index and without quote_free_confirmed => Speculative!
        let spec = Session::open_section(
            &utf16_path,
            window,
            SectionOpenOptions {
                open: OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    delimiter: Some(b','),
                    ..OpenOptions::default()
                },
                quote_free_confirmed: false,
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        let sb = spec.section_bounds().unwrap();
        assert_eq!(sb.trust, BoundaryTrust::Speculative);
        assert_eq!(sb.first_global_row(), None);
        assert!(sb.snapped.base.is_multiple_of(2));
        assert!(sb.snapped_end().is_multiple_of(2));

        // 2. With explicit quote_free_confirmed => Verified, STILL global row unknown!
        let qf = Session::open_section(
            &utf16_path,
            window,
            SectionOpenOptions {
                open: OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    delimiter: Some(b','),
                    ..OpenOptions::default()
                },
                quote_free_confirmed: true,
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        let qb = qf.section_bounds().unwrap();
        assert_eq!(qb.trust, BoundaryTrust::Verified);
        assert_eq!(qb.proof, BoundaryProof::QuoteFreeConfirmed);
        assert_eq!(qb.first_global_row(), None);

        // 3. VerifyBoundaryJob upgrades to CountedScan with exact global row ordinal.
        let verified = match spec
            .start_verify_section_boundary(None)
            .unwrap()
            .wait()
            .unwrap()
        {
            VerifyBoundaryOutcome::Verified { bounds, .. } => bounds,
            VerifyBoundaryOutcome::Cancelled => panic!("unexpected cancel"),
        };
        assert_eq!(verified.trust, BoundaryTrust::Verified);
        assert!(verified.first_global_row().is_some());
        assert_eq!(verified.snapped, sb.snapped);
    }

    #[test]
    fn speculative_multiline_boundary_shift_is_detected_by_pausable_verify_job() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multiline.csv");
        let mut data = Vec::from(&b"id,note\n"[..]);
        for i in 1..=200 {
            data.extend_from_slice(format!("{i},plain_{i}\n").as_bytes());
        }
        let quoted_start = data.len() as u64;
        data.extend_from_slice(b"201,\"line one of quoted field\n");
        let interior_lf_after = data.len() as u64;
        // Fill > 70 KiB of CSV-looking lines inside the multiline quoted field so the 64 KiB
        // bounded speculative probe legitimately accepts `interior_lf_after`, while the true
        // opening quote at `quoted_start` is only discovered by `VerifyBoundaryJob`.
        for k in 0..2600 {
            data.extend_from_slice(
                format!("{},inside_quote_{k:04}_padding_text\n", 500 + k).as_bytes(),
            );
        }
        data.extend_from_slice(b"closing_line\"\n");
        let quoted_end = data.len() as u64;
        for i in 202..=400 {
            data.extend_from_slice(format!("{i},after_{i}\n").as_bytes());
        }
        fs::write(&path, &data).unwrap();

        // Request a slice starting strictly inside the multiline quoted field right after its
        // interior LF so speculative alignment starts at `interior_lf_after`.
        let requested = ByteWindow::new(interior_lf_after + 4, 256).unwrap();
        let section = Session::open_section(
            &path,
            requested,
            SectionOpenOptions {
                open: OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    delimiter: Some(b','),
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            section.section_bounds().unwrap().trust,
            BoundaryTrust::Speculative
        );

        // Run VerifyBoundaryJob (exercising pause + resume on the job too) and confirm it finds
        // the true record start `quoted_start` and flags `boundary_changed == true`.
        let mut verify = section.start_verify_section_boundary(None).unwrap();
        verify.pause();
        verify.resume().unwrap();
        let outcome = verify.wait().unwrap();
        match outcome {
            VerifyBoundaryOutcome::Verified {
                bounds,
                boundary_changed,
            } => {
                assert_eq!(bounds.trust, BoundaryTrust::Verified);
                assert_eq!(bounds.first_global_row(), Some(201));
                assert_eq!(bounds.snapped.base, quoted_start);
                assert!(bounds.snapped_end() >= quoted_end);
                assert!(boundary_changed);
            }
            VerifyBoundaryOutcome::Cancelled => panic!("unexpected cancel"),
        }
    }

    #[test]
    fn speculative_open_fails_cleanly_when_no_boundary_within_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giant.csv");
        let mut data = Vec::from(&b"id,blob\n1,"[..]);
        data.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
        data.push(b'\n');
        fs::write(&path, &data).unwrap();

        let err = Session::open_section(
            &path,
            ByteWindow::new(1_500_000, 128).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    rows: 1,
                    header_mode: HeaderMode::FirstRow,
                    delimiter: Some(b','),
                    sample_bytes: 128,
                    bootstrap_limit: 128,
                },
                max_speculative_search_bytes: 64 * 1024,
                ..SectionOpenOptions::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(err, QuarryError::SectionBoundaryNotFound { .. }),
            "expected SectionBoundaryNotFound, got {err:?}"
        );
    }

    #[test]
    fn twenty_tib_sparse_file_section_open_index_read_and_save_section_as_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse-20tib.csv");
        let twenty_tib = 20u64 * 1024 * 1024 * 1024 * 1024;
        let eighteen_tib = 18u64 * 1024 * 1024 * 1024 * 1024;

        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        // Skip gracefully only if the test filesystem refuses 20 TiB sparse files (e.g. FAT/ext3).
        if f.set_len(twenty_tib).is_err() {
            return;
        }
        // Write header at byte 0.
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"id,region,amount\n0,boot,1.00\n").unwrap();

        // Write real CSV records near 18 TiB preceded by a clean LF boundary.
        let lead_lf = eighteen_tib - 1;
        f.seek(SeekFrom::Start(lead_lf)).unwrap();
        let payload = b"\n1001,arctic,42.50\n1002,tundra,99.25\n1003,boreal,12.00\n";
        f.write_all(payload).unwrap();
        f.flush().unwrap();
        drop(f);

        // Request an interior window intersecting row 1001 and row 1002 near 18 TiB.
        let req_start = eighteen_tib + 5;
        let req_end = eighteen_tib + 24;
        let section = Session::open_section(
            &path,
            ByteWindow::from_range(req_start, req_end).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    rows: 10,
                    header_mode: HeaderMode::FirstRow,
                    delimiter: Some(b','),
                    sample_bytes: 256,
                    bootstrap_limit: 256,
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();

        let bounds = section.section_bounds().unwrap();
        assert_eq!(bounds.trust, BoundaryTrust::Speculative);
        assert_eq!(bounds.first_global_row(), None);
        assert_eq!(bounds.snapped.base, eighteen_tib);
        assert_eq!(
            bounds.snapped_end(),
            eighteen_tib + b"1001,arctic,42.50\n1002,tundra,99.25\n".len() as u64
        );
        assert_eq!(section.section_header_row().unwrap().fields[0], b"id");
        assert_eq!(section.section_header_row().unwrap().fields[1], b"region");

        // Window-scoped StructuralIndex scans ONLY the ~36-byte section (instant, zero hole scan).
        let index = section
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(index.indexed_rows(), 2);
        let rows = section.read_rows(&index, 0, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].offset, eighteen_tib);
        assert_eq!(
            rows[0].fields,
            vec![b"1001".to_vec(), b"arctic".to_vec(), b"42.50".to_vec()]
        );
        assert_eq!(
            rows[1].fields,
            vec![b"1002".to_vec(), b"tundra".to_vec(), b"99.25".to_vec()]
        );

        // Save Section As New File writes the byte-0 header + edited section rows cleanly.
        let out_path = dir.path().join("exported-section.csv");
        let mut edits = BTreeMap::new();
        edits.insert((1u64, 2usize), b"100.00".to_vec());
        let outcome = section
            .start_save_as_with_edits(BTreeMap::new(), edits, &out_path)
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(outcome, SaveAsOutcome::Complete(_)));
        let exported = fs::read_to_string(&out_path).unwrap();
        assert_eq!(
            exported,
            "id,region,amount\n1001,arctic,42.50\n1002,tundra,100.00\n"
        );
    }

    #[test]
    fn synchronous_counted_scan_bounds_giant_record_beyond_requested_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giant-first-record.csv");
        // Write a 3 MiB single record without any newline; requesting [0, 64) from byte 0
        // enters the synchronous checkpoint path and must fail cleanly within `search_cap`
        // rather than scanning unboundedly.
        let mut data = b"a,b\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', 3 * 1024 * 1024));
        fs::write(&path, &data).unwrap();

        let err = Session::open_section(
            &path,
            ByteWindow::from_range(0, 64).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    delimiter: Some(b','),
                    ..OpenOptions::default()
                },
                max_speculative_search_bytes: 16 * 1024,
                ..SectionOpenOptions::default()
            },
        )
        .unwrap_err();
        assert!(
            matches!(err, QuarryError::SectionBoundaryNotFound { .. }),
            "expected bounded SectionBoundaryNotFound, got {err:?}"
        );
    }

    #[test]
    fn speculative_utf16_odd_end_offset_snaps_outward_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf16le-odd-end.csv");
        let mut text = String::from("id,val\n");
        for i in 0..40 {
            text.push_str(&format!("{i},row_value_{i:04}\n"));
        }
        let mut utf16 = vec![0xFFu8, 0xFEu8];
        for unit in text.encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(&path, &utf16).unwrap();

        // Force Tier 3 speculative snapping (`req_start > 0`, no parent index) with an odd
        // `req_end` inside the file and smaller than `search_cap`.
        let even_win = ByteWindow::from_range(100, 120).unwrap();
        let odd_win = ByteWindow::from_range(100, 121).unwrap();
        let make_opts = || SectionOpenOptions {
            open: OpenOptions {
                header_mode: HeaderMode::FirstRow,
                delimiter: Some(b','),
                ..OpenOptions::default()
            },
            encoding: Some(Encoding::Utf16Le),
            ..SectionOpenOptions::default()
        };

        let even_sec = Session::open_section(&path, even_win, make_opts()).unwrap();
        let odd_sec = Session::open_section(&path, odd_win, make_opts()).unwrap();
        let even_bounds = even_sec.section_bounds().unwrap();
        let odd_bounds = odd_sec.section_bounds().unwrap();
        assert_eq!(odd_bounds.trust, BoundaryTrust::Speculative);
        assert!(odd_bounds.snapped.base <= 100);
        assert!(odd_bounds.snapped_end() >= 121);
        assert_eq!(odd_bounds.snapped.base % 2, 0);
        assert_eq!(odd_bounds.snapped_end() % 2, 0);
        assert!(odd_bounds.snapped_end() >= even_bounds.snapped_end());
    }

    #[test]
    fn section_index_cannot_be_reused_as_whole_file_parent_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-section-parent.csv");
        let mut data = b"id,val\n".to_vec();
        for i in 0..500u64 {
            data.extend_from_slice(format!("{i},row_{i:04}_padding\n").as_bytes());
        }
        fs::write(&path, &data).unwrap();

        let sec1 = Session::open_section(
            &path,
            ByteWindow::from_range(200, 1200).unwrap(),
            SectionOpenOptions::default(),
        )
        .unwrap();
        let sec1_idx = sec1
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        assert!(!sec1_idx.is_whole_file());

        // Passing `sec1_idx` as `parent_index` must NOT mark a nested interior section as Verified
        // with bogus section-local global row numbers.
        let sec2 = Session::open_section(
            &path,
            ByteWindow::from_range(400, 800).unwrap(),
            SectionOpenOptions {
                parent_index: Some(sec1_idx),
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        let bounds2 = sec2.section_bounds().unwrap();
        assert_eq!(bounds2.trust, BoundaryTrust::Speculative);
        assert_eq!(bounds2.first_global_row(), None);
    }

    #[test]
    fn synchronous_counted_scan_stops_at_section_end_before_trailing_malformed_csv() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trailing-malformed.csv");
        fs::write(&path, b"a,b\nc,d\nbad\"quote,x\n").unwrap();

        // Requesting [0, 4) (`a,b\n`) with explicit comma and NoHeader must succeed without
        // parsing the malformed third row later in the same 256 KiB chunk.
        let sec = Session::open_section(
            &path,
            ByteWindow::from_range(0, 4).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::NoHeader,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        let bounds = sec.section_bounds().unwrap();
        assert_eq!(bounds.trust, BoundaryTrust::Verified);
        assert_eq!(bounds.snapped.base, 0);
        assert_eq!(bounds.snapped_end(), 4);
        assert_eq!(sec.first_rows.len(), 1);
        assert_eq!(sec.first_rows[0].fields, vec![b"a".to_vec(), b"b".to_vec()]);

        // FirstRow and Auto header probes must also stop after collecting their 1 or 2 records
        // so malformed row 3 later in the bootstrap chunk never breaks opening [0, 10).
        let path2 = dir.path().join("trailing-malformed-headered.csv");
        fs::write(&path2, b"h1,h2\na,b\nbad\"quote,x\n").unwrap();
        for mode in [HeaderMode::FirstRow, HeaderMode::Auto] {
            let sec_hdr = Session::open_section(
                &path2,
                ByteWindow::from_range(0, 10).unwrap(),
                SectionOpenOptions {
                    open: OpenOptions {
                        delimiter: Some(b','),
                        header_mode: mode,
                        ..OpenOptions::default()
                    },
                    ..SectionOpenOptions::default()
                },
            )
            .unwrap();
            assert_eq!(sec_hdr.section_bounds().unwrap().snapped_end(), 10);
            assert_eq!(sec_hdr.first_rows.len(), 2);
            assert_eq!(
                sec_hdr.first_rows[0].fields,
                vec![b"h1".to_vec(), b"h2".to_vec()]
            );
            assert_eq!(
                sec_hdr.first_rows[1].fields,
                vec![b"a".to_vec(), b"b".to_vec()]
            );
        }
    }

    #[test]
    fn filtered_export_and_paused_filter_limit_stay_within_section_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("filtered-section.csv");
        let mut data = b"id,tag\n".to_vec();
        for i in 1..=20u64 {
            data.extend_from_slice(format!("{i},match\n").as_bytes());
        }
        let sec_start = data.len() as u64;
        for i in 21..=30u64 {
            data.extend_from_slice(format!("{i},match\n").as_bytes());
        }
        let mid_section = data.len() as u64;
        for i in 31..=40u64 {
            data.extend_from_slice(format!("{i},match\n").as_bytes());
        }
        let sec_end = data.len() as u64;
        for i in 41..=60u64 {
            data.extend_from_slice(format!("{i},match\n").as_bytes());
        }
        fs::write(&path, &data).unwrap();

        let sec = Session::open_section(
            &path,
            ByteWindow::from_range(sec_start, sec_end).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();

        // 1. Paused filter with absolute `max_bytes = mid_section` must stop at `mid_section`
        // (10 rows: id 21..=30), NOT add `sec_start + mid_section` and scan the whole section.
        let query = crate::FilterQuery {
            predicates: vec![crate::FilterPredicate {
                column: 1,
                operator: crate::FilterOperator::Equals,
                value: "match".into(),
                upper_bound: None,
            }],
            case_sensitivity: crate::CaseSensitivity::Sensitive,
        };
        let partial_idx = sec
            .start_filter_with_edits_and_limit(query.clone(), Arc::default(), Some(mid_section))
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(partial_idx.matches_found(), 10);

        // 2. Filtered export of the section with a section-local edit on local row 0 (id=21)
        // must export ONLY the 20 matching rows inside `[sec_start, sec_end)` plus the byte-0
        // header, applying the edit to local row 0 (id=21) and never including rows 1..=20 or 41..=60.
        let out_path = dir.path().join("filtered-section-out.csv");
        let mut cell_edits: BTreeMap<(u64, usize), Arc<Vec<u8>>> = BTreeMap::new();
        cell_edits.insert((0u64, 1usize), Arc::new(b"match".to_vec()));
        cell_edits.insert((0u64, 0usize), Arc::new(b"EDITED_21".to_vec()));
        let outcome = sec
            .start_filtered_export_with_edits(
                query,
                Arc::new(cell_edits),
                BTreeMap::new(),
                &out_path,
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(outcome, crate::FilterExportOutcome::Complete(_)));
        let exported = fs::read_to_string(&out_path).unwrap();
        let lines: Vec<&str> = exported.lines().collect();
        assert_eq!(lines.len(), 21, "header + 20 section rows");
        assert_eq!(lines[0], "id,tag");
        assert_eq!(lines[1], "EDITED_21,match");
        assert_eq!(lines[20], "40,match");
        assert!(!exported.contains("\n1,match\n"));
        assert!(!exported.contains("\n41,match\n"));
    }

    #[test]
    fn headerless_interior_utf16le_bom_section_preserves_bom_on_save_and_filter_export() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf16le-bom-noheader.csv");
        let text = "r1,alpha\nr2,beta\nr3,beta\nr4,gamma\n";
        let mut raw = vec![0xFFu8, 0xFEu8];
        for unit in text.encode_utf16() {
            raw.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(&path, &raw).unwrap();

        let r2_offset = 2 + ("r1,alpha\n".encode_utf16().count() * 2) as u64;
        let r3_end = 2 + ("r1,alpha\nr2,beta\nr3,beta\n".encode_utf16().count() * 2) as u64;
        let sec = Session::open_section(
            &path,
            ByteWindow::from_range(r2_offset, r3_end).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::NoHeader,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(sec.encoding(), Encoding::Utf16Le);

        // Save Section As New preserves UTF-16LE BOM so Auto detection reopens cleanly.
        let save_out = dir.path().join("saved-utf16le.csv");
        let outcome = sec
            .start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), &save_out)
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(outcome, crate::SaveAsOutcome::Complete(_)));
        let saved_bytes = fs::read(&save_out).unwrap();
        assert!(
            saved_bytes.starts_with(&[0xFF, 0xFE]),
            "saved headerless UTF-16LE section must keep BOM"
        );
        let reopened = Session::open(
            &save_out,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(reopened.encoding(), Encoding::Utf16Le);
        assert_eq!(reopened.first_rows.len(), 2);
        assert_eq!(
            reopened.first_rows[0].fields,
            vec![b"r2".to_vec(), b"beta".to_vec()]
        );

        // Filtered Export also preserves the UTF-16LE BOM for headerless interior sections.
        let filter_out = dir.path().join("filtered-utf16le.csv");
        let query = crate::FilterQuery {
            predicates: vec![crate::FilterPredicate {
                column: 1,
                operator: crate::FilterOperator::Equals,
                value: "beta".into(),
                upper_bound: None,
            }],
            case_sensitivity: crate::CaseSensitivity::Sensitive,
        };
        let f_outcome = sec
            .start_filtered_export_with_edits(query, Arc::default(), BTreeMap::new(), &filter_out)
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(f_outcome, crate::FilterExportOutcome::Complete(_)));
        let filtered_bytes = fs::read(&filter_out).unwrap();
        assert!(
            filtered_bytes.starts_with(&[0xFF, 0xFE]),
            "filtered export of headerless UTF-16LE section must keep BOM"
        );
    }

    #[test]
    fn bomless_headerless_interior_section_protects_leading_feff_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bomless-feff.csv");
        let first_line = b"a,b\n";
        let sec_start = first_line.len() as u64;
        let second_line = "\"\u{FEFF}x\",old\n".as_bytes();
        let mut raw = Vec::new();
        raw.extend_from_slice(first_line);
        raw.extend_from_slice(second_line);
        raw.extend_from_slice(b"c,d\n");
        fs::write(&path, &raw).unwrap();

        let sec = Session::open_section(
            &path,
            ByteWindow::from_range(sec_start, sec_start + second_line.len() as u64).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::NoHeader,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        // Edit column 1 so row 0 is re-serialized, and verify the leading U+FEFF field stays quoted
        // (not mistaken for a UTF-8 file BOM on reopen).
        let out_path = dir.path().join("bomless-feff-out.csv");
        let mut edits = BTreeMap::new();
        edits.insert((0u64, 1usize), b"new".to_vec());
        let outcome = sec
            .start_save_as_with_edits(BTreeMap::new(), edits, &out_path)
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(outcome, crate::SaveAsOutcome::Complete(_)));
        let saved = fs::read(&out_path).unwrap();
        assert!(
            !saved.starts_with(&[0xEF, 0xBB, 0xBF]),
            "BOM-less section save must quote leading U+FEFF rather than emitting a raw BOM"
        );
        let reopened = Session::open(
            &out_path,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(reopened.first_rows[0].fields[0], "\u{FEFF}x".as_bytes());
        assert_eq!(reopened.first_rows[0].fields[1], b"new");
    }

    #[test]
    fn quote_free_confirmed_rejects_quoted_section_and_mismatched_indexes_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quoted-and-mismatch.csv");
        let mut raw = b"id,note\n".to_vec();
        for i in 0..400u64 {
            raw.extend_from_slice(format!("{i},\"quoted_{i}\"\n").as_bytes());
        }
        let mid = raw.len() as u64 / 2;
        fs::write(&path, &raw).unwrap();

        // 1. quote_free_confirmed must refuse when `"` quotes are present in the section.
        let err = Session::open_section(
            &path,
            ByteWindow::from_range(mid, mid + 256).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                quote_free_confirmed: true,
                ..SectionOpenOptions::default()
            },
        );
        assert!(
            err.is_err(),
            "quote_free_confirmed must reject sections containing quote characters"
        );

        // 2. Two sections of the same file must reject each other's StructuralIndex & FilterIndex,
        // and a byte-0 section filter must stay bounded to the section end.
        let sec_a = Session::open_section(
            &path,
            ByteWindow::from_range(0, 200).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        let sec_b = Session::open_section(
            &path,
            ByteWindow::from_range(mid, mid + 200).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();

        let idx_a = sec_a
            .start_indexing(crate::IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        assert!(
            matches!(
                sec_b.read_rows(&idx_a, 0, 5),
                Err(QuarryError::InvalidOption(_))
            ),
            "read_rows must reject a StructuralIndex from another section"
        );
        assert!(
            matches!(
                sec_b.start_search(
                    &idx_a,
                    b"quoted".to_vec(),
                    crate::SearchPosition { row: 0, column: 0 },
                ),
                Err(QuarryError::InvalidOption(_))
            ),
            "start_search must reject a StructuralIndex from another section"
        );

        let query = crate::FilterQuery {
            predicates: vec![crate::FilterPredicate {
                column: 1,
                operator: crate::FilterOperator::Contains,
                value: "quoted_".into(),
                upper_bound: None,
            }],
            case_sensitivity: crate::CaseSensitivity::Sensitive,
        };
        let f_idx_a = sec_a
            .start_filter_with_edits(query, Arc::default())
            .unwrap()
            .wait()
            .unwrap();
        assert!(
            f_idx_a.matches_found() < 40,
            "byte-0 section filter must stay bounded to section end, got {}",
            f_idx_a.matches_found()
        );
        assert!(
            matches!(
                sec_b.read_filtered_rows(&f_idx_a, 0, 5),
                Err(QuarryError::SourceChanged)
            ),
            "read_filtered_rows must reject a FilterIndex from another section"
        );
    }

    #[test]
    fn background_verify_stops_before_trailing_malformed_csv_in_same_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify-trailing-malformed.csv");
        let mut raw = b"a,b\nc,d\n".to_vec();
        for i in 0..5000u64 {
            raw.extend_from_slice(format!("{i:05},valid_padding_row\n").as_bytes());
        }
        raw.extend_from_slice(b"bad\"quote,x\n");
        fs::write(&path, &raw).unwrap();

        // Open interior `[4, 8)` (`c,d\n`) as Speculative (`a,b\n` precedes it, no parent index),
        // then run VerifyBoundaryJob: must stop at byte 8 and succeed without touching `bad"quote`.
        let sec = Session::open_section(
            &path,
            ByteWindow::from_range(4, 8).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::NoHeader,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            sec.section_bounds().unwrap().trust,
            BoundaryTrust::Speculative
        );

        let outcome = sec
            .start_verify_section_boundary(None)
            .unwrap()
            .wait()
            .unwrap();
        match outcome {
            VerifyBoundaryOutcome::Verified {
                bounds,
                boundary_changed,
            } => {
                assert!(!boundary_changed);
                assert_eq!(bounds.snapped.base, 4);
                assert_eq!(bounds.snapped_end(), 8);
                assert_eq!(bounds.first_global_row(), Some(1));
            }
            other => panic!("expected Verified outcome, got {other:?}"),
        }
    }

    #[test]
    fn interior_section_preserves_literal_leading_feff_across_bootstrap_filter_search_and_export() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interior-literal-feff.csv");
        let raw = "a,b\n\u{FEFF}keep,x\nlast,y\n".as_bytes();
        fs::write(&path, raw).unwrap();

        let sec_start = b"a,b\n".len() as u64;
        let sec_end = sec_start + "\u{FEFF}keep,x\n".len() as u64;
        let sec = Session::open_section(
            &path,
            ByteWindow::from_range(sec_start, sec_end).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::NoHeader,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();

        // 1. Bootstrap first_rows keeps literal U+FEFF.
        assert_eq!(sec.first_rows.len(), 1);
        assert_eq!(sec.first_rows[0].fields[0], "\u{FEFF}keep".as_bytes());

        // 2. Indexed read_rows and Search keep literal U+FEFF.
        let idx = sec
            .start_indexing(crate::IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        let rows = sec.read_rows(&idx, 0, 5).unwrap();
        assert_eq!(rows[0].fields[0], "\u{FEFF}keep".as_bytes());
        let search = sec
            .start_search(
                &idx,
                "\u{FEFF}keep".as_bytes().to_vec(),
                crate::SearchPosition { row: 0, column: 0 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(search, crate::SearchOutcome::Match(_)));

        // 3. Filter Equals("\u{FEFF}keep") matches, read_filtered_rows keeps it, and filtered export keeps it.
        let query = crate::FilterQuery {
            predicates: vec![crate::FilterPredicate {
                column: 0,
                operator: crate::FilterOperator::Equals,
                value: "\u{FEFF}keep".into(),
                upper_bound: None,
            }],
            case_sensitivity: crate::CaseSensitivity::Sensitive,
        };
        let f_idx = sec
            .start_filter_with_edits(query.clone(), Arc::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(f_idx.matches_found(), 1);
        let f_rows = sec.read_filtered_rows(&f_idx, 0, 5).unwrap();
        assert_eq!(f_rows[0].fields[0], "\u{FEFF}keep".as_bytes());

        let out_path = dir.path().join("interior-feff-filtered.csv");
        let outcome = sec
            .start_filtered_export_with_edits(query, Arc::default(), BTreeMap::new(), &out_path)
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(outcome, crate::FilterExportOutcome::Complete(_)));
        let reopened = Session::open(
            &out_path,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(reopened.first_rows[0].fields[0], "\u{FEFF}keep".as_bytes());
    }

    #[test]
    fn speculative_start_candidate_header_budget_preserved_framing_and_cross_file_index_guards() {
        let dir = tempfile::tempdir().unwrap();

        // 1. Speculative interior section start validation must stop at closing record covering
        // `req_end` and not be rejected by unrelated malformed CSV after the section.
        let path_spec = dir.path().join("spec-trailing-malformed.csv");
        let mut raw = Vec::with_capacity(1_200_100);
        for _ in 0..300_000 {
            raw.extend_from_slice(b"a,b\n");
        }
        let sec_start = raw.len() as u64;
        raw.extend_from_slice(b"c,d\n");
        let sec_end = raw.len() as u64;
        raw.extend_from_slice(b"bad\"quote,x\n");
        fs::write(&path_spec, &raw).unwrap();

        let sec = Session::open_section(
            &path_spec,
            ByteWindow::from_range(sec_start, sec_end).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::NoHeader,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(sec.section_bounds().unwrap().snapped.base, sec_start);
        assert_eq!(sec.section_bounds().unwrap().snapped_end(), sec_end);

        // 2. Exhausting bootstrap_limit during FirstRow/Auto header probe returns BootstrapLimitExceeded.
        let path_hdr = dir.path().join("short-budget-header.csv");
        fs::write(&path_hdr, b"header\nx\n").unwrap();
        for mode in [HeaderMode::FirstRow, HeaderMode::Auto] {
            let err = Session::open_section(
                &path_hdr,
                ByteWindow::from_range(7, 9).unwrap(),
                SectionOpenOptions {
                    open: OpenOptions {
                        delimiter: Some(b','),
                        header_mode: mode,
                        sample_bytes: 4,
                        bootstrap_limit: 4,
                        ..OpenOptions::default()
                    },
                    ..SectionOpenOptions::default()
                },
            );
            assert!(
                matches!(
                    err,
                    Err(QuarryError::BootstrapLimitExceeded { limit: 4, .. })
                ),
                "expected BootstrapLimitExceeded for {mode:?}, got {err:?}"
            );
        }

        // 3. Same-range index from a different file must be rejected by both read_rows and start_search.
        let path_a = dir.path().join("same-range-a.csv");
        let path_b = dir.path().join("same-range-b.csv");
        fs::write(&path_a, b"a\nb\nc\n").unwrap();
        fs::write(&path_b, b"a\nxyz\n").unwrap();
        let sa = Session::open_section(
            &path_a,
            ByteWindow::from_range(2, 6).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::NoHeader,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        let sb = Session::open_section(
            &path_b,
            ByteWindow::from_range(2, 6).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::NoHeader,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        let idx_a = sa
            .start_indexing(crate::IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(
            sb.read_rows(&idx_a, 0, 5),
            Err(QuarryError::InvalidOption(_))
        ));
        assert!(matches!(
            sb.start_search(
                &idx_a,
                b"z".to_vec(),
                crate::SearchPosition { row: 0, column: 0 }
            ),
            Err(QuarryError::InvalidOption(_))
        ));

        // 4. Section filtered export excludes synthesized header from rows_written and rejects OOB edits.
        let path_exp = dir.path().join("sec-exp-count.csv");
        fs::write(&path_exp, b"id,v\n1,ok\n").unwrap();
        let se = Session::open_section(
            &path_exp,
            ByteWindow::from_range(5, 10).unwrap(),
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        let no_match = crate::FilterQuery {
            predicates: vec![crate::FilterPredicate {
                column: 1,
                operator: crate::FilterOperator::Equals,
                value: "missing".into(),
                upper_bound: None,
            }],
            case_sensitivity: crate::CaseSensitivity::Sensitive,
        };
        let out_zero = dir.path().join("sec-exp-zero.csv");
        match se
            .start_filtered_export_with_edits(
                no_match.clone(),
                Arc::default(),
                BTreeMap::new(),
                &out_zero,
            )
            .unwrap()
            .wait()
            .unwrap()
        {
            crate::FilterExportOutcome::Complete(summary) => {
                assert_eq!(
                    summary.rows_written, 0,
                    "synthesized header must not be counted in rows_written"
                );
            }
            other => panic!("expected Complete, got {other:?}"),
        }
        let mut oob_edits = BTreeMap::new();
        oob_edits.insert((1u64, 0usize), Arc::new(b"bad".to_vec()));
        let out_oob = dir.path().join("sec-exp-oob.csv");
        let oob_res = se
            .start_filtered_export_with_edits(
                no_match,
                Arc::new(oob_edits),
                BTreeMap::new(),
                &out_oob,
            )
            .unwrap()
            .wait();
        assert!(
            matches!(oob_res, Err(QuarryError::InvalidOption(_))),
            "out-of-range cell edit on section filtered export must be rejected, got {oob_res:?}"
        );
        assert!(!out_oob.exists());
    }

    #[test]
    fn large_speculative_section_keeps_malformed_trailing_suffix_outside_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large_speculative_malformed_suffix.csv");
        // 600,000 × "a,b\n" = 2,400,000 bytes (> 1 MiB speculative window) + malformed trailing row.
        let mut bytes = Vec::with_capacity(2_400_064);
        for _ in 0..600_000 {
            bytes.extend_from_slice(b"a,b\n");
        }
        let clean_end = bytes.len() as u64;
        assert_eq!(clean_end, 2_400_000);
        bytes.extend_from_slice(b"bad\"quote,x\n");
        fs::write(&path, &bytes).unwrap();

        let opts = SectionOpenOptions {
            open: OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
            ..SectionOpenOptions::default()
        };

        // 1. Requested end right at the clean boundary before malformed trailing CSV.
        let sec_exact = Session::open_section(
            &path,
            ByteWindow::from_range(4, clean_end).unwrap(),
            opts.clone(),
        )
        .unwrap();
        let bounds_exact = sec_exact.section_bounds().unwrap();
        assert_eq!(bounds_exact.trust, BoundaryTrust::Speculative);
        assert_eq!(
            bounds_exact.snapped,
            ByteWindow::from_range(4, clean_end).unwrap(),
            "malformed trailing suffix must not extend large speculative section end"
        );
        let idx = sec_exact
            .start_indexing(crate::IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        assert!(idx.is_complete());
        assert_eq!(idx.indexed_rows(), 599_999);

        // 2. Requested end inside the final clean record before malformed trailing CSV.
        let sec_mid = Session::open_section(
            &path,
            ByteWindow::from_range(4, clean_end - 2).unwrap(),
            opts.clone(),
        )
        .unwrap();
        assert_eq!(
            sec_mid.section_bounds().unwrap().snapped,
            ByteWindow::from_range(4, clean_end).unwrap()
        );

        // 3. Multiline quoted record crossing req_end, followed by malformed trailing CSV.
        let path_ml = dir.path().join("large_speculative_multiline_suffix.csv");
        let mut ml_bytes = Vec::with_capacity(2_400_128);
        for _ in 0..600_000 {
            ml_bytes.extend_from_slice(b"a,b\n");
        }
        ml_bytes.extend_from_slice(b"\"line1\n");
        let mid_quoted_end = ml_bytes.len() as u64;
        ml_bytes.extend_from_slice(b"line2\",c\n");
        let closing_quoted_end = ml_bytes.len() as u64;
        ml_bytes.extend_from_slice(b"bad\"quote,x\n");
        fs::write(&path_ml, &ml_bytes).unwrap();

        let sec_ml = Session::open_section(
            &path_ml,
            ByteWindow::from_range(4, mid_quoted_end).unwrap(),
            opts,
        )
        .unwrap();
        assert_eq!(
            sec_ml.section_bounds().unwrap().snapped,
            ByteWindow::from_range(4, closing_quoted_end).unwrap(),
            "multiline quoted record must snap to its own closing LF without pulling in malformed suffix"
        );
    }

    #[test]
    fn large_speculative_single_record_over_1mib_preserves_exact_end_and_rejects_interior_lf() {
        let dir = tempfile::tempdir().unwrap();
        let opts = SectionOpenOptions {
            open: OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
            ..SectionOpenOptions::default()
        };

        // 1. Single > 1 MiB record ending exactly at req_end, followed by valid CSV.
        let path_exact = dir.path().join("single_record_over_1mib.csv");
        let mut bytes = Vec::with_capacity(1_200_064);
        bytes.extend_from_slice(b"a,b\n");
        bytes.extend_from_slice(b"1,");
        bytes.resize(bytes.len() + 1_200_000, b'x');
        bytes.push(b'\n');
        let exact_end = bytes.len() as u64;
        bytes.extend_from_slice(b"2,ok\n");
        fs::write(&path_exact, &bytes).unwrap();

        let sec = Session::open_section(
            &path_exact,
            ByteWindow::from_range(4, exact_end).unwrap(),
            opts.clone(),
        )
        .unwrap();
        let bounds = sec.section_bounds().unwrap();
        assert_eq!(bounds.trust, BoundaryTrust::Speculative);
        assert_eq!(
            bounds.snapped,
            ByteWindow::from_range(4, exact_end).unwrap(),
            "exact record end after a > 1 MiB record must be preserved"
        );

        // 2. Requesting a mid-record offset inside that > 1 MiB record still fails cleanly.
        let mid_err = Session::open_section(
            &path_exact,
            ByteWindow::from_range(4, exact_end - 10).unwrap(),
            opts.clone(),
        );
        assert!(
            matches!(mid_err, Err(QuarryError::SectionBoundaryNotFound { .. })),
            "mid-record offset with no anchor within 1 MiB must fail, got {mid_err:?}"
        );

        // 3. > 1 MiB multiline quoted record where req_end sits on an interior LF before closing quote.
        let path_quoted = dir.path().join("quoted_record_over_1mib_interior_lf.csv");
        let mut qbytes = Vec::with_capacity(1_200_128);
        qbytes.extend_from_slice(b"a,b\n");
        qbytes.extend_from_slice(b"1,\"");
        qbytes.resize(qbytes.len() + 1_200_000, b'q');
        qbytes.push(b'\n');
        let interior_lf_end = qbytes.len() as u64;
        qbytes.extend_from_slice(b"still inside quote\nclosing\",ok\n2,after\n");
        fs::write(&path_quoted, &qbytes).unwrap();

        let quoted_err = Session::open_section(
            &path_quoted,
            ByteWindow::from_range(4, interior_lf_end).unwrap(),
            opts.clone(),
        );
        assert!(
            matches!(quoted_err, Err(QuarryError::SectionBoundaryNotFound { .. })),
            "interior LF inside > 1 MiB quoted field must not be accepted as a section end, got {quoted_err:?}"
        );

        // 4. Valid > 1 MiB record ending at req_end followed by a > 64 KiB valid next record.
        let path_long_next = dir.path().join("single_record_over_1mib_long_next.csv");
        let mut ln_bytes = Vec::with_capacity(1_300_000);
        ln_bytes.extend_from_slice(b"a,b\n");
        ln_bytes.extend_from_slice(b"1,");
        ln_bytes.resize(ln_bytes.len() + 1_200_000, b'x');
        ln_bytes.push(b'\n');
        let ln_end = ln_bytes.len() as u64;
        ln_bytes.extend_from_slice(b"2,");
        ln_bytes.resize(ln_bytes.len() + 70_000, b'y');
        ln_bytes.push(b'\n');
        fs::write(&path_long_next, &ln_bytes).unwrap();

        let sec_ln = Session::open_section(
            &path_long_next,
            ByteWindow::from_range(4, ln_end).unwrap(),
            opts.clone(),
        )
        .unwrap();
        assert_eq!(
            sec_ln.section_bounds().unwrap().snapped,
            ByteWindow::from_range(4, ln_end).unwrap(),
            "exact end followed by a valid > 64 KiB record must be preserved"
        );

        // 5. > 1 MiB multiline quoted record whose closing quote lies > 64 KiB after the interior LF.
        let path_quoted_70k = dir.path().join("quoted_record_over_1mib_70k_after_lf.csv");
        let mut q70 = Vec::with_capacity(1_300_000);
        q70.extend_from_slice(b"a,b\n");
        q70.extend_from_slice(b"1,\"");
        q70.resize(q70.len() + 1_200_000, b'q');
        q70.push(b'\n');
        let q70_lf_end = q70.len() as u64;
        q70.resize(q70.len() + 70_000, b'q');
        q70.extend_from_slice(b"\",ok\n2,after\n");
        fs::write(&path_quoted_70k, &q70).unwrap();

        let q70_err = Session::open_section(
            &path_quoted_70k,
            ByteWindow::from_range(4, q70_lf_end).unwrap(),
            opts.clone(),
        );
        assert!(
            matches!(q70_err, Err(QuarryError::SectionBoundaryNotFound { .. })),
            "interior LF with closing quote > 64 KiB later must be rejected, got {q70_err:?}"
        );

        // 6. > 1 MiB multiline quoted record whose closing quote lies > 1 MiB after the interior LF
        // (forward probe completes zero records within search_cap and must fail closed).
        let path_quoted_far = dir.path().join("quoted_record_over_1mib_far_after_lf.csv");
        let mut qfar = Vec::with_capacity(2_500_000);
        qfar.extend_from_slice(b"a,b\n");
        qfar.extend_from_slice(b"1,\"");
        qfar.resize(qfar.len() + 1_200_000, b'q');
        qfar.push(b'\n');
        let qfar_lf_end = qfar.len() as u64;
        qfar.resize(qfar.len() + 1_200_000, b'q');
        qfar.extend_from_slice(b"\",ok\n2,after\n");
        fs::write(&path_quoted_far, &qfar).unwrap();

        let qfar_err = Session::open_section(
            &path_quoted_far,
            ByteWindow::from_range(4, qfar_lf_end).unwrap(),
            opts,
        );
        assert!(
            matches!(qfar_err, Err(QuarryError::SectionBoundaryNotFound { .. })),
            "inconclusive forward probe completing zero records within search_cap must fail closed, got {qfar_err:?}"
        );
    }
}
