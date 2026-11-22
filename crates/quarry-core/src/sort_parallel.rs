//! Parallel external sort, adaptive single-run memory arena, and bounded parallel merges.

use std::borrow::Cow;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use quarry_delimited::parse_record;

use super::{
    DuplicateSpec, MERGE_MEMORY_BUDGET_BYTES, NUMERIC_KEY_OVERHEAD, OutputVerifier,
    PendingOutputEntry, RecordMultisetFingerprint, RunOrder, SharedState, SortConfig,
    SortDirection, SortPlan, SortVerification, UTF8_BOM, delimited_field_len,
    effective_merge_fan_in, merge_runs_to_run_with_dedup, record_ending, serialize_fields,
    write_entry_parts, write_output, write_verified_output_entry,
};
use crate::Encoding;
use crate::encoding::RecordScanner;
use crate::export::ExportTarget;
use crate::pool::{
    BudgetGuard, ByteBudget, FdBudget, JobExecutor, JobPriority, LARGE_RECORD_GATE_BYTES,
    LargeRecordGate, LargeRecordGuard, engine_pool,
};
use crate::positional_io::{advise_readahead, read_at_exact_or_eof};
use crate::spill::{SpillFile, SpillWorkspace as RunWorkspace};
use crate::{QuarryError, StructuralIndex};

pub(super) const DEFAULT_MIN_PARALLEL_STRIPE_BYTES: u64 = 64 * 1024;
const MAX_PARALLEL_STRIPE_BYTES: u64 = 32 * 1024 * 1024;
const ARENA_ENTRY_BYTES: usize = std::mem::size_of::<ArenaEntry>();
const BUDGET_REFILL_QUANTUM: usize = 256 * 1024;
const MERGE_STREAM_BUFFER_BYTES: usize = 8 * 1024;
/// Cap steady-state external spill run-generation buffers across all workers so multi-GB
/// external sorts stay well inside the 256 MiB RSS envelope while still needing <= 2 merge passes.
pub(super) const EXTERNAL_SPILL_TOTAL_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone)]
pub(super) enum AdaptiveRunOrder {
    MultiSort(Arc<SortPlan>),
    Duplicates(Arc<DuplicateSpec>),
}

impl AdaptiveRunOrder {
    pub(super) fn direction(&self) -> SortDirection {
        match self {
            Self::MultiSort(plan) => plan.direction(),
            Self::Duplicates(_) => SortDirection::Ascending,
        }
    }

    pub(super) fn as_run_order(&self) -> RunOrder<'_> {
        match self {
            Self::MultiSort(plan) => RunOrder::MultiSort(plan),
            Self::Duplicates(spec) => RunOrder::Duplicates(spec),
        }
    }

    pub(super) fn is_composite_key(&self) -> bool {
        match self {
            Self::MultiSort(plan) => plan.keys.len() > 1,
            Self::Duplicates(spec) => spec.columns.len() > 1,
        }
    }

    pub(super) fn key_overhead_bytes(&self) -> usize {
        match self {
            Self::MultiSort(plan) => plan
                .keys
                .len()
                .saturating_mul(NUMERIC_KEY_OVERHEAD.saturating_add(2)),
            Self::Duplicates(spec) => spec.columns.len().saturating_mul(8),
        }
    }

    pub(super) fn dedup_on_flush(&self) -> bool {
        matches!(self, Self::Duplicates(_))
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ArenaEntry {
    pub(super) key_offset: usize,
    pub(super) key_len: usize,
    pub(super) record_offset: usize,
    pub(super) record_len: usize,
    pub(super) ordinal: u64,
}

/// Contiguous payload arena holding sorted or unsorted run entries without per-row heap allocations.
pub(super) struct RunArena {
    bytes: Vec<u8>,
    entries: Vec<ArenaEntry>,
    sorted: bool,
}

impl RunArena {
    pub(super) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            entries: Vec::new(),
            sorted: false,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn capacity_bytes(&self) -> usize {
        self.bytes
            .capacity()
            .saturating_add(self.entries.capacity().saturating_mul(ARENA_ENTRY_BYTES))
    }

    pub(super) const fn entry_footprint(key_len: usize, record_len: usize) -> usize {
        key_len
            .saturating_add(record_len)
            .saturating_add(ARENA_ENTRY_BYTES)
    }

    /// Compute `(next_bytes_cap, next_entries_cap)` that `push_bounded` will allocate for
    /// appending `(key_len, record_len)` given an optional combined byte cap.
    ///
    /// Whenever the resulting lengths fit inside `combined_cap_hint`, the returned combined
    /// capacity `next_bytes_cap + next_entries_cap * ARENA_ENTRY_BYTES` stays `<= combined_cap_hint`.
    /// For a single oversized entry that exceeds `combined_cap_hint`, returns the exact minimum
    /// capacities needed (`target_bytes_len`, `target_entries_len`).
    pub(super) fn plan_growth(
        &self,
        key_len: usize,
        record_len: usize,
        combined_cap_hint: Option<usize>,
    ) -> (usize, usize) {
        let needed_bytes = key_len.saturating_add(record_len);
        let target_bytes_len = self.bytes.len().saturating_add(needed_bytes);
        let cur_bytes_cap = self.bytes.capacity();
        let cur_entries_cap = self.entries.capacity();
        let target_entries_len = self.entries.len().saturating_add(1);

        let avg_payload = target_bytes_len.div_ceil(target_entries_len).max(1);
        let bytes_per_entry = ARENA_ENTRY_BYTES.saturating_add(avg_payload);

        let next_bytes_cap = if target_bytes_len <= cur_bytes_cap {
            cur_bytes_cap
        } else if let Some(cap) = combined_cap_hint {
            let min_entries_bytes = target_entries_len
                .max(cur_entries_cap)
                .saturating_mul(ARENA_ENTRY_BYTES);
            let bytes_ceiling = cap.saturating_sub(min_entries_bytes);
            if target_bytes_len <= bytes_ceiling {
                let balanced_entries = (cap / bytes_per_entry).max(target_entries_len);
                let balanced_entries_bytes = balanced_entries
                    .max(cur_entries_cap)
                    .saturating_mul(ARENA_ENTRY_BYTES);
                let balanced_bytes_ceiling = cap
                    .saturating_sub(balanced_entries_bytes)
                    .max(target_bytes_len)
                    .min(bytes_ceiling);
                cur_bytes_cap
                    .saturating_mul(2)
                    .max(target_bytes_len)
                    .min(balanced_bytes_ceiling)
            } else {
                target_bytes_len
            }
        } else {
            cur_bytes_cap.saturating_mul(2).max(target_bytes_len)
        };

        let next_entries_cap = if target_entries_len <= cur_entries_cap {
            cur_entries_cap
        } else if let Some(cap) = combined_cap_hint {
            let remaining_bytes = cap.saturating_sub(next_bytes_cap);
            let max_entries_by_space = remaining_bytes / ARENA_ENTRY_BYTES;
            let balanced_max_entries = (cap / bytes_per_entry).max(target_entries_len);
            let max_entries = max_entries_by_space.min(balanced_max_entries);
            if target_entries_len <= max_entries {
                cur_entries_cap
                    .saturating_mul(2)
                    .max(64)
                    .min(max_entries)
                    .max(target_entries_len)
            } else {
                target_entries_len
            }
        } else {
            cur_entries_cap
                .saturating_mul(2)
                .max(64)
                .max(target_entries_len)
        };

        (next_bytes_cap, next_entries_cap)
    }

    pub(super) fn planned_capacity_bytes(
        &self,
        key_len: usize,
        record_len: usize,
        combined_cap_hint: Option<usize>,
    ) -> usize {
        let (next_bytes_cap, next_entries_cap) =
            self.plan_growth(key_len, record_len, combined_cap_hint);
        next_bytes_cap.saturating_add(next_entries_cap.saturating_mul(ARENA_ENTRY_BYTES))
    }

    pub(super) fn push_bounded(
        &mut self,
        key: &[u8],
        record: &[u8],
        ordinal: u64,
        combined_cap_hint: Option<usize>,
    ) {
        let (next_bytes_cap, next_entries_cap) =
            self.plan_growth(key.len(), record.len(), combined_cap_hint);
        if next_bytes_cap > self.bytes.capacity() {
            self.bytes.reserve_exact(next_bytes_cap - self.bytes.len());
        }
        let key_offset = self.bytes.len();
        self.bytes.extend_from_slice(key);
        let record_offset = self.bytes.len();
        self.bytes.extend_from_slice(record);
        if next_entries_cap > self.entries.capacity() {
            self.entries
                .reserve_exact(next_entries_cap - self.entries.len());
        }
        self.entries.push(ArenaEntry {
            key_offset,
            key_len: key.len(),
            record_offset,
            record_len: record.len(),
            ordinal,
        });
        self.sorted = false;
    }

    pub(super) fn key_at(&self, index: usize) -> &[u8] {
        let entry = self.entries[index];
        &self.bytes[entry.key_offset..entry.key_offset + entry.key_len]
    }

    pub(super) fn record_at(&self, index: usize) -> &[u8] {
        let entry = self.entries[index];
        &self.bytes[entry.record_offset..entry.record_offset + entry.record_len]
    }

    pub(super) fn ordinal_at(&self, index: usize) -> u64 {
        self.entries[index].ordinal
    }

    pub(super) fn sort(&mut self, direction: SortDirection) {
        if self.sorted {
            return;
        }
        let bytes = &self.bytes;
        // `compare_key_ordinal` breaks all ties with unique source ordinals, so the order is
        // strict and `sort_unstable_by` matches `sort_by` identically with O(1) stack scratch.
        self.entries.sort_unstable_by(|left, right| {
            let l_key = &bytes[left.key_offset..left.key_offset + left.key_len];
            let r_key = &bytes[right.key_offset..right.key_offset + right.key_len];
            compare_key_ordinal(l_key, left.ordinal, r_key, right.ordinal, direction)
        });
        self.sorted = true;
    }

    /// Clear arena contents while reusing steady-state buffers whose combined capacity stays
    /// within `retain_bytes_cap` so repeated spill flushes avoid allocator thrashing without
    /// exceeding the worker's budgeted capacity.
    pub(super) fn clear_for_reuse(&mut self, retain_bytes_cap: usize) {
        let prev_bytes_len = self.bytes.len();
        self.entries.clear();
        self.bytes.clear();
        if retain_bytes_cap == 0
            || self.capacity_bytes() > retain_bytes_cap
            || (prev_bytes_len < self.bytes.capacity() / 4
                && self.bytes.capacity() > retain_bytes_cap / 2)
        {
            self.bytes.shrink_to(0);
            self.entries.shrink_to(0);
        }
        self.sorted = false;
    }

    pub(super) fn flush_to_workspace_mode(
        &mut self,
        direction: SortDirection,
        dedup: bool,
        workspace: &LazyRunWorkspace,
        shared: &SharedState,
        retain_bytes_cap: usize,
    ) -> Result<Option<(SpillFile, u64)>, QuarryError> {
        if self.entries.is_empty() {
            return Ok(None);
        }
        debug_assert!(!dedup || direction == SortDirection::Ascending);
        self.sort(direction);
        let (path, mut writer) = workspace.create_run()?;
        let mut dropped = 0_u64;
        let mut prev_key: Option<(usize, usize)> = None;
        for (idx, entry) in self.entries.iter().enumerate() {
            if (idx & 0x3ff) == 0 && shared.cancel_requested.load(Ordering::Acquire) {
                return Ok(None);
            }
            let key = &self.bytes[entry.key_offset..entry.key_offset + entry.key_len];
            if dedup && prev_key.is_some_and(|(off, len)| &self.bytes[off..off + len] == key) {
                dropped = dropped.saturating_add(1);
                continue;
            }
            let record = &self.bytes[entry.record_offset..entry.record_offset + entry.record_len];
            let bytes = write_entry_parts(&mut writer, key, record, entry.ordinal)?;
            shared.add_temporary_bytes(bytes);
            if dedup {
                prev_key = Some((entry.key_offset, entry.key_len));
            }
        }
        writer.flush()?;
        self.clear_for_reuse(retain_bytes_cap);
        shared.runs_created.fetch_add(1, Ordering::AcqRel);
        Ok(Some((path, dropped)))
    }
}

pub(super) fn compare_key_ordinal(
    left_key: &[u8],
    left_ordinal: u64,
    right_key: &[u8],
    right_ordinal: u64,
    direction: SortDirection,
) -> CmpOrdering {
    let key_order = match direction {
        SortDirection::Ascending => left_key.cmp(right_key),
        SortDirection::Descending => right_key.cmp(left_key),
    };
    key_order.then_with(|| left_ordinal.cmp(&right_ordinal))
}

/// Creates the private spill workspace lazily on first spill so in-memory fast-path sorts
/// create zero temporary run directories or files on disk.
pub(super) struct LazyRunWorkspace {
    parent: PathBuf,
    workspace: Mutex<Option<RunWorkspace>>,
}

impl LazyRunWorkspace {
    pub(super) fn new(parent: PathBuf) -> Self {
        Self {
            parent,
            workspace: Mutex::new(None),
        }
    }

    pub(super) fn create_run(&self) -> Result<(SpillFile, io::BufWriter<File>), QuarryError> {
        let mut guard = self.workspace.lock().unwrap();
        if guard.is_none() {
            *guard = Some(RunWorkspace::create_in(&self.parent, "sort")?);
        }
        guard.as_ref().unwrap().create_run()
    }

    pub(super) fn cleanup(self) -> Result<(), QuarryError> {
        if let Some(workspace) = self.workspace.into_inner().unwrap() {
            workspace.cleanup()?;
        }
        Ok(())
    }
}

/// Job-level payload budget backed by an up-front reservation from the engine pool's shared
/// `out_of_core_budget`. Because the reservation is acquired on the coordinator thread before
/// spawning workers onto `EnginePool`, worker threads never block on cross-job budget waits
/// and cannot starve queued tasks belonging to other jobs.
pub(super) struct JobMemoryBudget {
    limit: usize,
    shared_guard: Mutex<Option<BudgetGuard>>,
    used: Mutex<usize>,
    available: Condvar,
    spilled: AtomicBool,
}

pub(super) struct JobBudgetGuard {
    job: Arc<JobMemoryBudget>,
    reserved: usize,
    used: usize,
}

impl Drop for JobBudgetGuard {
    fn drop(&mut self) {
        if self.reserved > 0 {
            let mut used = self.job.used.lock().unwrap();
            *used = used.saturating_sub(self.reserved);
            if self.job.spilled.load(Ordering::Acquire) {
                self.job.shrink_shared_reservation_locked(*used);
            }
            self.job.available.notify_all();
        }
    }
}

impl JobMemoryBudget {
    /// Reserve a job-scoped slice of `shared` on the coordinator thread before spawning workers.
    /// Returns `None` if `cancelled` fires while waiting for shared budget admission.
    pub(super) fn acquire_for_job(
        requested_limit: usize,
        shared: &Arc<ByteBudget>,
        cancelled: &AtomicBool,
    ) -> Option<Arc<Self>> {
        let preferred = requested_limit.max(1);
        let min_bytes = preferred.clamp(1, EXTERNAL_SPILL_TOTAL_BYTES / 8);
        let shared_guard = shared.acquire_up_to(min_bytes, preferred, cancelled)?;
        let granted_limit = shared_guard.bytes().min(preferred).max(1);
        Some(Arc::new(Self {
            limit: granted_limit,
            shared_guard: Mutex::new(Some(shared_guard)),
            used: Mutex::new(0),
            available: Condvar::new(),
            spilled: AtomicBool::new(false),
        }))
    }

    pub(super) fn limit(&self) -> usize {
        self.limit
    }

    fn effective_limit(&self) -> usize {
        if self.spilled.load(Ordering::Acquire) {
            self.limit.clamp(1, EXTERNAL_SPILL_TOTAL_BYTES)
        } else {
            self.limit
        }
    }

    fn shrink_shared_reservation_locked(&self, current_used: usize) {
        if let Some(guard) = self.shared_guard.lock().unwrap().as_mut() {
            // Never release shared budget still held by live arenas; shrink down toward
            // `EXTERNAL_SPILL_TOTAL_BYTES` only as live arena reservations are dropped.
            let steady_target = self.limit.clamp(1, EXTERNAL_SPILL_TOTAL_BYTES);
            let target = current_used.max(steady_target);
            guard.shrink_to(target);
        }
    }

    pub(super) fn is_spilled(&self) -> bool {
        self.spilled.load(Ordering::Acquire)
    }

    pub(super) fn mark_spilled(&self) {
        if !self.spilled.swap(true, Ordering::AcqRel) {
            let used = self.used.lock().unwrap();
            self.shrink_shared_reservation_locked(*used);
        }
    }

    /// Shrink the coordinator's shared reservation down to `target` (never below live `used`)
    /// when transitioning into a smaller post-scan phase such as survivor-run creation.
    pub(super) fn shrink_shared_reservation_to(&self, target: usize) {
        let current_used = *self.used.lock().unwrap();
        if let Some(guard) = self.shared_guard.lock().unwrap().as_mut() {
            guard.shrink_to(current_used.max(target.max(1)));
        }
    }

    /// Release the coordinator's shared budget reservation once arenas are no longer in memory.
    pub(super) fn release_shared_reservation(&self) {
        self.shared_guard.lock().unwrap().take();
    }

    /// Non-blocking reservation that succeeds only when `used + bytes <= effective_limit`
    /// (never admitting above-budget allocations). Used for optional fast-path auxiliary
    /// allocations such as the multi-arena duplicate survivor vector.
    pub(super) fn try_acquire_within_limit(
        self: &Arc<Self>,
        bytes: usize,
    ) -> Option<JobBudgetGuard> {
        if bytes == 0 {
            return Some(JobBudgetGuard {
                job: Arc::clone(self),
                reserved: 0,
                used: 0,
            });
        }
        let mut used = self.used.lock().unwrap();
        let effective_limit = self.effective_limit();
        if used.saturating_add(bytes) > effective_limit {
            return None;
        }
        *used = used.saturating_add(bytes);
        Some(JobBudgetGuard {
            job: Arc::clone(self),
            reserved: bytes,
            used: bytes,
        })
    }

    /// Non-blocking reservation attempt for an empty worker arena. Returns `None` when
    /// another worker of this job currently holds budget so the caller can spill completed arenas first.
    pub(super) fn try_acquire_empty(self: &Arc<Self>, bytes: usize) -> Option<JobBudgetGuard> {
        let needed = bytes.max(1);
        let mut used = self.used.lock().unwrap();
        let effective_limit = self.effective_limit();
        if *used != 0 && used.saturating_add(needed) > effective_limit {
            return None;
        }
        *used = used.saturating_add(needed);
        Some(JobBudgetGuard {
            job: Arc::clone(self),
            reserved: needed,
            used: needed,
        })
    }

    /// Blocking reservation used when a worker's arena is empty so even an oversized row
    /// makes forward progress once other active workers of this same job flush.
    pub(super) fn acquire_empty(
        self: &Arc<Self>,
        bytes: usize,
        cancelled: &AtomicBool,
    ) -> Option<JobBudgetGuard> {
        self.acquire_empty_with_spill(|_| bytes, cancelled, || Ok(true))
            .ok()
            .flatten()
            .map(|(guard, _)| guard)
    }

    /// Reservation for an empty worker arena that invokes `on_wait` before waiting on this
    /// job's active workers (allowing parallel workers to drain and spill `completed_arenas`).
    /// Recomputes `compute_bytes(is_spilled)` on every retry so a waiting worker immediately
    /// switches from the pre-spill plan to the smaller spill-cap plan after a spill transition.
    /// Returns `(guard, spilled_used)` so the caller appends with the exact plan that was reserved.
    /// Never blocks on cross-job `EnginePool` budgets.
    pub(super) fn acquire_empty_with_spill(
        self: &Arc<Self>,
        mut compute_bytes: impl FnMut(bool) -> usize,
        cancelled: &AtomicBool,
        mut on_wait: impl FnMut() -> Result<bool, QuarryError>,
    ) -> Result<Option<(JobBudgetGuard, bool)>, QuarryError> {
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
            let spilled = self.is_spilled();
            let needed = compute_bytes(spilled).max(1);
            if let Some(guard) = self.try_acquire_empty(needed) {
                return Ok(Some((guard, spilled)));
            }
            if !on_wait()? {
                return Ok(None);
            }
            let spilled = self.is_spilled();
            let needed = compute_bytes(spilled).max(1);
            if let Some(guard) = self.try_acquire_empty(needed) {
                return Ok(Some((guard, spilled)));
            }
            let used = self.used.lock().unwrap();
            let effective_limit = self.effective_limit();
            if *used == 0
                || used.saturating_add(needed) <= effective_limit
                || self.is_spilled() != spilled
            {
                continue;
            }
            let _ = self
                .available
                .wait_timeout(used, std::time::Duration::from_millis(2))
                .unwrap();
        }
    }
}

impl JobBudgetGuard {
    pub(super) fn used(&self) -> usize {
        self.used
    }

    /// Shrink this guard's tracked and reserved budget down to `new_used`, releasing excess
    /// bytes back to the job budget immediately.
    pub(super) fn shrink_used_to(&mut self, new_used: usize) {
        self.used = new_used;
        if self.reserved > new_used {
            let release = self.reserved - new_used;
            self.reserved = new_used;
            let mut used = self.job.used.lock().unwrap();
            *used = used.saturating_sub(release);
            if self.job.spilled.load(Ordering::Acquire) {
                self.job.shrink_shared_reservation_locked(*used);
            }
            self.job.available.notify_all();
        }
    }

    /// Non-blocking reservation for adding another row to a non-empty arena.
    /// Never blocks so concurrent workers cannot deadlock; returns `false` to trigger spill.
    pub(super) fn try_reserve_additional(
        &mut self,
        extra: usize,
        worker_cap: Option<usize>,
    ) -> bool {
        let new_used = self.used.saturating_add(extra);
        if let Some(cap) = worker_cap
            && new_used > cap
        {
            return false;
        }
        if new_used <= self.reserved {
            self.used = new_used;
            return true;
        }
        let exact_deficit = new_used - self.reserved;
        let effective_limit = self.job.effective_limit();
        let quantum = if effective_limit >= 4 * 1024 * 1024 {
            let max_quantum = worker_cap
                .map(|cap| cap.saturating_sub(self.reserved))
                .unwrap_or(effective_limit.saturating_sub(self.reserved));
            exact_deficit
                .max(BUDGET_REFILL_QUANTUM)
                .min(max_quantum.max(exact_deficit))
        } else {
            exact_deficit
        };
        if self.try_grow_by(quantum)
            || (quantum != exact_deficit && self.try_grow_by(exact_deficit))
        {
            self.used = new_used;
            true
        } else {
            false
        }
    }

    fn try_grow_by(&mut self, delta: usize) -> bool {
        if delta == 0 {
            return true;
        }
        let mut used = self.job.used.lock().unwrap();
        let effective_limit = self.job.effective_limit();
        if used.saturating_add(delta) > effective_limit {
            return false;
        }
        *used = used.saturating_add(delta);
        self.reserved = self.reserved.saturating_add(delta);
        true
    }
}

/// Shared verified output writer used identically by both the in-memory fast path and external merge.
pub(super) struct VerifiedOutputWriter<'a> {
    output: &'a mut ExportTarget,
    preferred_ending: &'a [u8],
    max_record_bytes: usize,
    prior_bytes_written: u64,
    first_record_prefix_bytes: u64,
    shared: &'a SharedState,
    output_index: Option<&'a mut StructuralIndex>,
    verifier: OutputVerifier,
    pending: Option<PendingOutputEntry>,
    rows: u64,
    bytes_written: u64,
}

impl<'a> VerifiedOutputWriter<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        output: &'a mut ExportTarget,
        preferred_ending: &'a [u8],
        expected_records: RecordMultisetFingerprint,
        max_record_bytes: usize,
        prior_bytes_written: u64,
        first_record_prefix_bytes: u64,
        shared: &'a SharedState,
        output_index: Option<&'a mut StructuralIndex>,
    ) -> Self {
        Self {
            output,
            preferred_ending,
            max_record_bytes,
            prior_bytes_written,
            first_record_prefix_bytes,
            shared,
            output_index,
            verifier: OutputVerifier::new(expected_records),
            pending: None,
            rows: 0,
            bytes_written: 0,
        }
    }

    /// Verify adjacency against the upcoming `(next_key, next_ordinal)` and write + drop `pending`
    /// BEFORE reading/allocating the next record so at most one record payload is retained in RAM.
    pub(super) fn flush_previous_before(
        &mut self,
        next_key: &[u8],
        next_ordinal: u64,
    ) -> Result<(), QuarryError> {
        if let Some(previous) = self.pending.take() {
            self.verifier.verify_adjacent(
                &previous.key,
                previous.ordinal,
                next_key,
                next_ordinal,
            )?;
            let inserted_ending = if previous.record.ends_with(b"\n") {
                b"".as_slice()
            } else {
                self.preferred_ending
            };
            if previous.record.len().saturating_add(inserted_ending.len()) > self.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: self.max_record_bytes,
                });
            }
            let written = write_verified_output_entry(
                self.output,
                previous,
                inserted_ending,
                self.first_record_prefix_bytes,
                &mut self.verifier,
                self.shared,
            )?;
            self.first_record_prefix_bytes = 0;
            self.bytes_written = self.bytes_written.saturating_add(written);
            if let Some(index) = &mut self.output_index {
                index.record_boundary(self.prior_bytes_written.saturating_add(self.bytes_written));
            }
            self.shared.bytes_written.store(
                self.prior_bytes_written.saturating_add(self.bytes_written),
                Ordering::Release,
            );
        }
        Ok(())
    }

    pub(super) fn set_pending(&mut self, key: Vec<u8>, record: Vec<u8>, ordinal: u64) {
        self.pending = Some(PendingOutputEntry {
            key,
            record,
            ordinal,
        });
        self.rows = self.rows.saturating_add(1);
    }

    pub(super) fn finish(mut self) -> Result<(u64, u64, SortVerification), QuarryError> {
        if let Some(last) = self.pending.take() {
            let written = write_verified_output_entry(
                self.output,
                last,
                b"",
                self.first_record_prefix_bytes,
                &mut self.verifier,
                self.shared,
            )?;
            self.bytes_written = self.bytes_written.saturating_add(written);
            if let Some(index) = self.output_index {
                index.record_boundary(self.prior_bytes_written.saturating_add(self.bytes_written));
            }
            self.shared.bytes_written.store(
                self.prior_bytes_written.saturating_add(self.bytes_written),
                Ordering::Release,
            );
        }
        let verification = self.verifier.finish()?;
        Ok((self.rows, self.bytes_written, verification))
    }
}

pub(super) struct InMemoryRun {
    pub(super) arena: RunArena,
    pub(super) _budget_guard: Option<JobBudgetGuard>,
}

pub(super) struct AdaptiveScanSummary {
    pub(super) header: Option<Vec<u8>>,
    pub(super) preferred_ending: Vec<u8>,
    pub(super) runs: Vec<SpillFile>,
    pub(super) in_memory_runs: Vec<InMemoryRun>,
    pub(super) rows: u64,
    pub(super) max_key_bytes: usize,
    pub(super) maximum_width: usize,
    pub(super) records: RecordMultisetFingerprint,
    pub(super) effective_workers: usize,
}

pub(super) enum AdaptiveScanOutcome {
    Complete(AdaptiveScanSummary),
    Cancelled,
}

#[derive(Clone, Copy)]
struct SortStripe {
    start_row: u64,
    end_row: Option<u64>,
    start_offset: u64,
    end_offset: u64,
    is_eof: bool,
}

fn build_sort_stripes(
    index: &StructuralIndex,
    file_size: u64,
    effective_workers: usize,
    min_stripe_bytes: u64,
) -> Option<Vec<SortStripe>> {
    if index.indexed_bytes() != file_size || index.indexed_rows() == 0 {
        return None;
    }
    let checkpoints = index.checkpoints();
    let first = checkpoints.first().copied()?;
    if first.row != 0 || first.offset != 0 {
        return None;
    }
    let target = (file_size / (effective_workers.saturating_mul(4).max(1) as u64))
        .clamp(min_stripe_bytes.max(1), MAX_PARALLEL_STRIPE_BYTES);

    let mut stripes = Vec::new();
    let mut cur = first;
    for &cp in &checkpoints[1..] {
        if cp.offset <= cur.offset || cp.row <= cur.row || cp.offset > file_size {
            return None;
        }
        if cp.offset - cur.offset >= target {
            stripes.push(SortStripe {
                start_row: cur.row,
                end_row: Some(cp.row),
                start_offset: cur.offset,
                end_offset: cp.offset,
                is_eof: cp.offset == file_size,
            });
            cur = cp;
        }
    }
    if cur.offset < file_size {
        stripes.push(SortStripe {
            start_row: cur.row,
            end_row: Some(index.indexed_rows()),
            start_offset: cur.offset,
            end_offset: file_size,
            is_eof: true,
        });
    }
    if stripes.len() >= 2 {
        Some(stripes)
    } else {
        None
    }
}

/// Process one logical record into either header retention or `(key, effective_record, ordinal)`
/// using the exact same rules across serial and parallel run generation.
enum ProcessedRecord<'a> {
    Header(Vec<u8>),
    Data {
        key: Vec<u8>,
        record: Cow<'a, [u8]>,
        ordinal: u64,
    },
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
fn evaluate_record<'a>(
    raw_record: &'a [u8],
    physical_row: u64,
    delimiter: u8,
    has_header: bool,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    order: RunOrder<'_>,
    bom_present: bool,
    config: SortConfig,
    cancel_requested: &AtomicBool,
    first_ending: &mut Option<(u64, Vec<u8>)>,
    maximum_width: &mut usize,
) -> Result<ProcessedRecord<'a>, QuarryError> {
    if raw_record.len() > config.max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: config.max_record_bytes,
        });
    }
    let decoded = config
        .encoding
        .decode(raw_record, config.max_record_bytes)
        .map_err(|error| crate::record_encoding_error(physical_row.saturating_add(1), error))?;
    let body_is_borrowed = matches!(&decoded, Cow::Borrowed(_));
    let record = decoded.as_ref();
    let bom_stripped = physical_row == 0 && bom_present && record.starts_with(UTF8_BOM);
    let body = if bom_stripped {
        &record[UTF8_BOM.len()..]
    } else {
        record
    };
    let ending = record_ending(body);
    if !ending.is_empty()
        && first_ending
            .as_ref()
            .is_none_or(|(row, _)| physical_row < *row)
    {
        *first_ending = Some((physical_row, ending.to_vec()));
    }
    let fields = parse_record(body, delimiter)?;
    *maximum_width = (*maximum_width).max(fields.len());

    if has_header && physical_row == 0 {
        if header_renames
            .last_key_value()
            .is_some_and(|(column, _)| *column >= fields.len())
        {
            return Err(QuarryError::InvalidOption(
                "header rename column is out of range",
            ));
        }
        let header = if header_renames.is_empty() {
            body.to_vec()
        } else {
            let mut values: Vec<Vec<u8>> = fields.iter().map(|field| field.to_vec()).collect();
            for (column, value) in header_renames {
                values[*column] = value.clone();
            }
            serialize_fields(&values, delimiter, ending, config.max_record_bytes)?
        };
        if header
            .len()
            .saturating_add(usize::from(bom_present) * UTF8_BOM.len())
            > config.max_record_bytes
        {
            return Err(QuarryError::RecordTooLarge {
                limit: config.max_record_bytes,
            });
        }
        return Ok(ProcessedRecord::Header(header));
    }

    let ordinal = physical_row.saturating_sub(u64::from(has_header));
    let row_edits: Vec<(usize, &[u8])> = cell_edits
        .range((physical_row, 0)..=(physical_row, usize::MAX))
        .map(|((_, column), value)| (*column, value.as_slice()))
        .collect();
    if row_edits.iter().any(|(column, _)| *column >= fields.len()) {
        return Err(QuarryError::InvalidOption(
            "cell edit column is out of range",
        ));
    }
    let value_at = |column| {
        row_edits
            .binary_search_by_key(&column, |(edited_column, _)| *edited_column)
            .ok()
            .map(|index| row_edits[index].1)
            .or_else(|| fields.get(column).map(|field| field.as_ref()))
            .unwrap_or_default()
    };
    if !row_edits.is_empty() {
        let mut serialized_bytes = ending
            .len()
            .saturating_add(fields.len().saturating_sub(1))
            .saturating_add(usize::from(physical_row == 0 && bom_present) * UTF8_BOM.len());
        for column in 0..fields.len() {
            if cancel_requested.load(Ordering::Acquire) {
                return Ok(ProcessedRecord::Cancelled);
            }
            let value = value_at(column);
            serialized_bytes = serialized_bytes.saturating_add(delimited_field_len(
                value,
                delimiter,
                column == 0 && value.starts_with(UTF8_BOM)
                    || fields.len() == 1 && value.is_empty() && ending.is_empty(),
            ));
            if serialized_bytes > config.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
        }
    }
    let key = match order {
        RunOrder::Duplicates(spec) => spec.key(value_at, config.max_record_bytes)?,
        #[cfg(test)]
        RunOrder::Sort(spec) => SortPlan::from(spec).key(value_at, ordinal)?,
        RunOrder::MultiSort(spec) => spec.key(value_at, ordinal)?,
    };
    let effective_record: Cow<'a, [u8]> = if row_edits.is_empty()
        && !(matches!(order, RunOrder::MultiSort(_))
            && physical_row > 0
            && body.starts_with(UTF8_BOM))
    {
        if body_is_borrowed {
            let borrowed = if bom_stripped {
                &raw_record[UTF8_BOM.len()..]
            } else {
                raw_record
            };
            Cow::Borrowed(borrowed)
        } else {
            Cow::Owned(body.to_vec())
        }
    } else {
        let mut values: Vec<Vec<u8>> = fields.iter().map(|field| field.to_vec()).collect();
        for (column, value) in row_edits {
            values[column] = value.to_vec();
        }
        Cow::Owned(serialize_fields(
            &values,
            delimiter,
            ending,
            config.max_record_bytes,
        )?)
    };
    if physical_row == 0
        && bom_present
        && effective_record.len().saturating_add(UTF8_BOM.len()) > config.max_record_bytes
    {
        return Err(QuarryError::RecordTooLarge {
            limit: config.max_record_bytes,
        });
    }

    Ok(ProcessedRecord::Data {
        key,
        record: effective_record,
        ordinal,
    })
}

struct ParallelCollector {
    header: Option<Vec<u8>>,
    first_ending: Option<(u64, Vec<u8>)>,
    spilled_runs: Vec<SpillFile>,
    completed_arenas: Vec<InMemoryRun>,
    rows: u64,
    physical_rows: u64,
    max_key_bytes: usize,
    maximum_width: usize,
    records: RecordMultisetFingerprint,
    earliest_error: Option<(u64, QuarryError)>,
}

impl ParallelCollector {
    fn new() -> Self {
        Self {
            header: None,
            first_ending: None,
            spilled_runs: Vec::new(),
            completed_arenas: Vec::new(),
            rows: 0,
            physical_rows: 0,
            max_key_bytes: 0,
            maximum_width: 0,
            records: RecordMultisetFingerprint::default(),
            earliest_error: None,
        }
    }

    fn record_error(&mut self, physical_row: u64, error: QuarryError, earliest_row: &AtomicU64) {
        earliest_row.fetch_min(physical_row, Ordering::AcqRel);
        if self
            .earliest_error
            .as_ref()
            .is_none_or(|(row, _)| physical_row < *row)
        {
            self.earliest_error = Some((physical_row, error));
        }
    }
}

fn clear_record_buffer(record: &mut Vec<u8>) {
    record.clear();
    if record.capacity() > LARGE_RECORD_GATE_BYTES {
        record.shrink_to(0);
    }
}

/// Sum the byte length of any unsaved edits targeting `physical_row` (O(1) when unedited).
fn row_edit_bytes(
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    has_header: bool,
    physical_row: u64,
) -> usize {
    if has_header && physical_row == 0 {
        if header_renames.is_empty() {
            return 0;
        }
        return header_renames
            .values()
            .fold(0_usize, |acc, v| acc.saturating_add(v.len()));
    }
    if cell_edits.is_empty() {
        return 0;
    }
    cell_edits
        .range((physical_row, 0)..=(physical_row, usize::MAX))
        .fold(0_usize, |acc, (_, v)| acc.saturating_add(v.len()))
}

/// Conservative upper bound on decoded record + effective record + sort-key + parsed field
/// descriptor memory before `evaluate_record` allocates, accounting for encoding expansion
/// (Windows-1252, Latin-1, UTF-16), cell edits (with CSV quote doubling), multi-column
/// composite-key escaping (`0x00`/`0xFF` byte doubling), and `parse_record`'s `Vec<Cow<[u8]>>`
/// field descriptors (with capacity-doubling headroom) so workers acquire `LargeRecordGate`
/// prior to allocation.
pub(super) fn conservative_record_expansion_bound(
    raw_record: &[u8],
    delimiter: u8,
    edit_bytes: usize,
    encoding: Encoding,
    composite_key: bool,
) -> usize {
    let raw_len = raw_record.len();
    let decode_multiplier = match encoding {
        Encoding::Windows1252 => 3,
        Encoding::Latin1 | Encoding::Utf16Le | Encoding::Utf16Be => 2,
        Encoding::Utf8 => 1,
    };
    let decoded_bound = raw_len.saturating_mul(decode_multiplier);
    let temp_decode_buffer = if encoding == Encoding::Utf8 {
        0
    } else {
        decoded_bound
    };
    let effective_record_bound = if edit_bytes > 0 {
        decoded_bound.saturating_add(edit_bytes).saturating_mul(2)
    } else {
        decoded_bound
    };
    let key_multiplier = if composite_key { 2 } else { 1 };
    let key_bound = decoded_bound
        .saturating_add(edit_bytes)
        .saturating_mul(key_multiplier);
    // For rows >= 64 KiB, count actual delimiters with SIMD `memchr` so wide text rows with
    // few columns do not falsely serialize while delimiter-heavy rows account for every
    // `Cow<[u8]>` descriptor (plus ~2x Vec capacity-doubling headroom).
    let field_count_bound = if raw_len >= 64 * 1024 {
        match encoding {
            Encoding::Utf8 | Encoding::Latin1 | Encoding::Windows1252 => {
                memchr::memchr_iter(delimiter, raw_record)
                    .count()
                    .saturating_add(1)
            }
            Encoding::Utf16Le | Encoding::Utf16Be => (raw_len / 2).saturating_add(1),
        }
    } else {
        raw_len.saturating_add(1)
    };
    let field_descriptor_bound = field_count_bound
        .saturating_mul(std::mem::size_of::<Cow<'static, [u8]>>())
        .saturating_mul(2);
    temp_decode_buffer
        .saturating_add(effective_record_bound)
        .saturating_add(key_bound)
        .saturating_add(field_descriptor_bound)
        .saturating_add(ARENA_ENTRY_BYTES)
}

fn record_flushed_run(
    run: SpillFile,
    dropped: u64,
    early_duplicates: Option<&AtomicU64>,
    runs: &mut Vec<SpillFile>,
) {
    if dropped > 0
        && let Some(counter) = early_duplicates
    {
        counter.fetch_add(dropped, Ordering::AcqRel);
    }
    runs.push(run);
}

fn sync_budget_after_flush(arena: &RunArena, budget_guard: &mut Option<JobBudgetGuard>) {
    let retained_cap = arena.capacity_bytes();
    if retained_cap == 0 {
        budget_guard.take();
    } else if let Some(guard) = budget_guard.as_mut() {
        guard.shrink_used_to(retained_cap);
    }
}

fn spill_collected_arenas(
    collector: &Mutex<ParallelCollector>,
    direction: SortDirection,
    dedup: bool,
    early_duplicates: Option<&AtomicU64>,
    workspace: &LazyRunWorkspace,
    shared: &SharedState,
) -> Result<bool, QuarryError> {
    let drained = {
        let mut guard = collector.lock().unwrap();
        std::mem::take(&mut guard.completed_arenas)
    };
    let mut new_runs = Vec::with_capacity(drained.len());
    for mut item in drained {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(false);
        }
        if let Some((run, dropped)) = item
            .arena
            .flush_to_workspace_mode(direction, dedup, workspace, shared, 0)?
        {
            record_flushed_run(run, dropped, early_duplicates, &mut new_runs);
        }
        drop(item);
    }
    if !new_runs.is_empty() {
        collector.lock().unwrap().spilled_runs.extend(new_runs);
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn create_initial_runs_adaptive(
    source: Arc<File>,
    file_size: u64,
    delimiter: u8,
    has_header: bool,
    header_renames: Arc<BTreeMap<usize, Vec<u8>>>,
    cell_edits: Arc<BTreeMap<(u64, usize), Vec<u8>>>,
    order: AdaptiveRunOrder,
    bom_present: bool,
    config: SortConfig,
    workspace: Arc<LazyRunWorkspace>,
    shared: Arc<SharedState>,
    source_index: Option<&StructuralIndex>,
    requested_workers: usize,
    min_parallel_stripe_bytes: u64,
    job_budget: Arc<JobMemoryBudget>,
    large_record_gate: Arc<LargeRecordGate>,
    early_duplicates: Option<Arc<AtomicU64>>,
) -> Result<AdaptiveScanOutcome, QuarryError> {
    let parallel_eligible = requested_workers > 1
        && matches!(
            config.encoding,
            Encoding::Utf8 | Encoding::Latin1 | Encoding::Windows1252
        );
    // When an unedited single-byte source strictly exceeds the single-run budget plus max header
    // size, it cannot fit in memory; switch straight to steady-state external spill sizing so peak
    // RSS stays well within the bounded envelope without a transient 300 MB pre-spill spike.
    if cell_edits.is_empty()
        && matches!(
            config.encoding,
            Encoding::Utf8 | Encoding::Latin1 | Encoding::Windows1252
        )
        && file_size > (job_budget.limit() as u64).saturating_add(config.max_record_bytes as u64)
    {
        job_budget.mark_spilled();
    }
    let stripes = if parallel_eligible {
        source_index.and_then(|index| {
            build_sort_stripes(
                index,
                file_size,
                requested_workers,
                min_parallel_stripe_bytes,
            )
        })
    } else {
        None
    };

    if let Some(stripes) = stripes {
        create_initial_runs_parallel(
            source,
            delimiter,
            has_header,
            header_renames,
            cell_edits,
            order,
            bom_present,
            config,
            workspace,
            shared,
            stripes,
            requested_workers,
            job_budget,
            large_record_gate,
            early_duplicates,
        )
    } else {
        create_initial_runs_serial(
            &source,
            delimiter,
            has_header,
            &header_renames,
            &cell_edits,
            &order,
            bom_present,
            config,
            &workspace,
            &shared,
            &job_budget,
            &large_record_gate,
            early_duplicates.as_deref(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn create_initial_runs_serial(
    source: &File,
    delimiter: u8,
    has_header: bool,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    order: &AdaptiveRunOrder,
    bom_present: bool,
    config: SortConfig,
    workspace: &LazyRunWorkspace,
    shared: &SharedState,
    job_budget: &Arc<JobMemoryBudget>,
    large_record_gate: &Arc<LargeRecordGate>,
    early_duplicates: Option<&AtomicU64>,
) -> Result<AdaptiveScanOutcome, QuarryError> {
    let mut reader = source.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut scanner = RecordScanner::new(delimiter, config.encoding)?;
    let mut chunk = vec![0_u8; config.encoding.chunk_size(config.chunk_bytes)];
    let mut record = Vec::new();
    let mut absolute_start = 0_u64;
    let mut physical_row = 0_u64;

    let mut header = None;
    let mut first_ending: Option<(u64, Vec<u8>)> = None;
    let mut maximum_width = 0_usize;
    let mut max_key_bytes = 0_usize;
    let mut rows = 0_u64;
    let mut records = RecordMultisetFingerprint::default();
    let mut runs = Vec::new();
    let mut arena = RunArena::new();
    let mut budget_guard: Option<JobBudgetGuard> = None;
    let mut large_guard: Option<LargeRecordGuard> = None;
    let direction = order.direction();
    let dedup = order.dedup_on_flush();
    let composite_key = order.is_composite_key();
    let key_overhead = order.key_overhead_bytes();

    let flush_serial = |arena: &mut RunArena,
                        runs: &mut Vec<SpillFile>,
                        budget_guard: &mut Option<JobBudgetGuard>,
                        retain_bytes_cap: usize|
     -> Result<bool, QuarryError> {
        let Some((run, dropped)) =
            arena.flush_to_workspace_mode(direction, dedup, workspace, shared, retain_bytes_cap)?
        else {
            return Ok(false);
        };
        record_flushed_run(run, dropped, early_duplicates, runs);
        sync_budget_after_flush(arena, budget_guard);
        Ok(true)
    };

    let serial_spill_cap = job_budget.limit().clamp(1, EXTERNAL_SPILL_TOTAL_BYTES);
    let mut handle_record = |raw_record: &[u8],
                             row_num: u64,
                             arena: &mut RunArena,
                             runs: &mut Vec<SpillFile>,
                             budget_guard: &mut Option<JobBudgetGuard>,
                             large_guard: &mut Option<LargeRecordGuard>|
     -> Result<bool, QuarryError> {
        let pre_bound = conservative_record_expansion_bound(
            raw_record,
            delimiter,
            row_edit_bytes(cell_edits, header_renames, has_header, row_num),
            config.encoding,
            composite_key,
        )
        .saturating_add(key_overhead);
        if (pre_bound > LARGE_RECORD_GATE_BYTES || pre_bound > job_budget.limit())
            && large_guard.is_none()
        {
            if let Some(g) = large_record_gate.try_acquire() {
                *large_guard = Some(g);
            } else {
                if !arena.is_empty() {
                    job_budget.mark_spilled();
                    if !flush_serial(arena, runs, budget_guard, 0)? {
                        return Ok(false);
                    }
                } else {
                    arena.clear_for_reuse(0);
                    budget_guard.take();
                }
                let Some(g) = large_record_gate.acquire(&shared.cancel_requested) else {
                    return Ok(false);
                };
                *large_guard = Some(g);
            }
        }
        let evaluated = evaluate_record(
            raw_record,
            row_num,
            delimiter,
            has_header,
            header_renames,
            cell_edits,
            order.as_run_order(),
            bom_present,
            config,
            &shared.cancel_requested,
            &mut first_ending,
            &mut maximum_width,
        )?;
        match evaluated {
            ProcessedRecord::Cancelled => return Ok(false),
            ProcessedRecord::Header(hdr) => {
                header = Some(hdr);
                shared.header_rows.store(1, Ordering::Release);
            }
            ProcessedRecord::Data {
                key,
                record: effective_record,
                ordinal,
            } => {
                max_key_bytes = max_key_bytes.max(key.len());
                let footprint = RunArena::entry_footprint(key.len(), effective_record.len());
                let needs_large =
                    footprint > LARGE_RECORD_GATE_BYTES || footprint > job_budget.limit();
                if needs_large && large_guard.is_none() {
                    if let Some(g) = large_record_gate.try_acquire() {
                        *large_guard = Some(g);
                    } else {
                        if !arena.is_empty() {
                            job_budget.mark_spilled();
                            if !flush_serial(arena, runs, budget_guard, 0)? {
                                return Ok(false);
                            }
                        } else {
                            arena.clear_for_reuse(0);
                            budget_guard.take();
                        }
                        let Some(g) = large_record_gate.acquire(&shared.cancel_requested) else {
                            return Ok(false);
                        };
                        *large_guard = Some(g);
                    }
                }
                if !arena.is_empty() {
                    let spilled = job_budget.is_spilled();
                    let active_cap = if spilled {
                        serial_spill_cap
                    } else {
                        job_budget.limit()
                    };
                    let planned = arena.planned_capacity_bytes(
                        key.len(),
                        effective_record.len(),
                        Some(active_cap),
                    );
                    let current = budget_guard.as_ref().map_or(0, JobBudgetGuard::used);
                    let delta = planned.saturating_sub(current);
                    let fits = planned <= active_cap
                        && budget_guard.as_mut().is_some_and(|g| {
                            g.try_reserve_additional(delta, spilled.then_some(serial_spill_cap))
                        });
                    if !fits {
                        job_budget.mark_spilled();
                        if !flush_serial(arena, runs, budget_guard, serial_spill_cap)? {
                            return Ok(false);
                        }
                    }
                }
                if arena.is_empty() {
                    let spilled = job_budget.is_spilled();
                    let active_cap = if spilled {
                        serial_spill_cap
                    } else {
                        job_budget.limit()
                    };
                    if let Some(guard) = budget_guard.as_mut() {
                        let planned = arena.planned_capacity_bytes(
                            key.len(),
                            effective_record.len(),
                            Some(active_cap),
                        );
                        let delta = planned.saturating_sub(guard.used());
                        if planned > active_cap
                            || !guard
                                .try_reserve_additional(delta, spilled.then_some(serial_spill_cap))
                        {
                            arena.clear_for_reuse(0);
                            budget_guard.take();
                        }
                    }
                    if budget_guard.is_none() {
                        let planned = arena.planned_capacity_bytes(
                            key.len(),
                            effective_record.len(),
                            Some(active_cap),
                        );
                        let Some(guard) =
                            job_budget.acquire_empty(planned, &shared.cancel_requested)
                        else {
                            return Ok(false);
                        };
                        *budget_guard = Some(guard);
                    }
                }
                records.observe(&effective_record);
                let cap_hint = if job_budget.is_spilled() {
                    Some(serial_spill_cap)
                } else {
                    Some(job_budget.limit())
                };
                arena.push_bounded(&key, &effective_record, ordinal, cap_hint);
                rows = rows.saturating_add(1);
                shared.rows_sorted.store(rows, Ordering::Release);
                if (large_guard.is_some()
                    && (job_budget.is_spilled() || arena.capacity_bytes() > job_budget.limit()))
                    || (job_budget.is_spilled() && arena.capacity_bytes() > serial_spill_cap)
                {
                    job_budget.mark_spilled();
                    if !flush_serial(arena, runs, budget_guard, 0)? {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    };

    loop {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(AdaptiveScanOutcome::Cancelled);
        }
        let read = config.encoding.read_chunk(&mut reader, &mut chunk)?;
        if read == 0 {
            let mut deferred_error = None;
            let mut cancelled = false;
            let finish_result = scanner.finish(absolute_start, |_| {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    cancelled = true;
                } else {
                    match handle_record(
                        &record,
                        physical_row,
                        &mut arena,
                        &mut runs,
                        &mut budget_guard,
                        &mut large_guard,
                    ) {
                        Ok(true) => {}
                        Ok(false) => cancelled = true,
                        Err(error) => deferred_error = Some(error),
                    }
                }
                clear_record_buffer(&mut record);
                large_guard.take();
                physical_row = physical_row.saturating_add(1);
            });
            clear_record_buffer(&mut record);
            large_guard.take();
            shared
                .bytes_scanned
                .store(absolute_start, Ordering::Release);
            shared.preparing.store(false, Ordering::Release);
            if cancelled {
                return Ok(AdaptiveScanOutcome::Cancelled);
            }
            if let Some(error) = deferred_error {
                return Err(error);
            }
            finish_result?;
            break;
        }

        let mut segment_start = 0_usize;
        let mut deferred_error = None;
        let mut cancelled = false;
        let scan_result = scanner.scan_chunk(&chunk[..read], absolute_start, |absolute_end| {
            let local_end = (absolute_end - absolute_start) as usize;
            if deferred_error.is_none() && !cancelled {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    cancelled = true;
                } else {
                    let slice = &chunk[segment_start..local_end];
                    if record.len().saturating_add(slice.len()) > config.max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: config.max_record_bytes,
                        });
                    } else {
                        record.extend_from_slice(slice);
                        match handle_record(
                            &record,
                            physical_row,
                            &mut arena,
                            &mut runs,
                            &mut budget_guard,
                            &mut large_guard,
                        ) {
                            Ok(true) => {}
                            Ok(false) => cancelled = true,
                            Err(error) => deferred_error = Some(error),
                        }
                    }
                }
            }
            clear_record_buffer(&mut record);
            large_guard.take();
            physical_row = physical_row.saturating_add(1);
            segment_start = local_end;
        });
        absolute_start = absolute_start.saturating_add(read as u64);
        shared
            .bytes_scanned
            .store(absolute_start, Ordering::Release);
        shared.preparing.store(false, Ordering::Release);
        if cancelled || shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(AdaptiveScanOutcome::Cancelled);
        }
        if let Some(error) = deferred_error {
            return Err(error);
        }
        scan_result?;
        let remainder = &chunk[segment_start..read];
        if record.len().saturating_add(remainder.len()) > config.max_record_bytes {
            return Err(QuarryError::RecordTooLarge {
                limit: config.max_record_bytes,
            });
        }
        if record.len().saturating_add(remainder.len()) > LARGE_RECORD_GATE_BYTES
            && large_guard.is_none()
        {
            if let Some(g) = large_record_gate.try_acquire() {
                large_guard = Some(g);
            } else {
                if !arena.is_empty() {
                    job_budget.mark_spilled();
                    if !flush_serial(&mut arena, &mut runs, &mut budget_guard, 0)? {
                        return Ok(AdaptiveScanOutcome::Cancelled);
                    }
                } else {
                    arena.clear_for_reuse(0);
                    budget_guard.take();
                }
                let Some(g) = large_record_gate.acquire(&shared.cancel_requested) else {
                    return Ok(AdaptiveScanOutcome::Cancelled);
                };
                large_guard = Some(g);
            }
        }
        record.extend_from_slice(remainder);
    }

    if has_header && header.is_none() {
        return Err(QuarryError::InvalidOption(
            "source does not contain a header row",
        ));
    }
    if cell_edits
        .last_key_value()
        .is_some_and(|((row, _), _)| *row >= physical_row)
    {
        return Err(QuarryError::InvalidOption("cell edit row is out of range"));
    }

    let mut in_memory_runs = Vec::new();
    if !arena.is_empty() {
        if !runs.is_empty() || job_budget.is_spilled() {
            if !flush_serial(&mut arena, &mut runs, &mut budget_guard, 0)? {
                return Ok(AdaptiveScanOutcome::Cancelled);
            }
        } else {
            arena.sort(direction);
            if let Some(guard) = budget_guard.as_mut() {
                guard.shrink_used_to(arena.capacity_bytes());
            }
            in_memory_runs.push(InMemoryRun {
                arena,
                _budget_guard: budget_guard,
            });
        }
    } else {
        arena.clear_for_reuse(0);
        budget_guard.take();
    }

    Ok(AdaptiveScanOutcome::Complete(AdaptiveScanSummary {
        header,
        preferred_ending: first_ending.map_or_else(|| b"\n".to_vec(), |(_, ending)| ending),
        runs,
        in_memory_runs,
        rows,
        max_key_bytes,
        maximum_width,
        records,
        effective_workers: 1,
    }))
}

#[allow(clippy::too_many_arguments)]
fn create_initial_runs_parallel(
    source: Arc<File>,
    delimiter: u8,
    has_header: bool,
    header_renames: Arc<BTreeMap<usize, Vec<u8>>>,
    cell_edits: Arc<BTreeMap<(u64, usize), Vec<u8>>>,
    order: AdaptiveRunOrder,
    bom_present: bool,
    config: SortConfig,
    workspace: Arc<LazyRunWorkspace>,
    shared: Arc<SharedState>,
    stripes: Vec<SortStripe>,
    requested_workers: usize,
    job_budget: Arc<JobMemoryBudget>,
    large_record_gate: Arc<LargeRecordGate>,
    early_duplicates: Option<Arc<AtomicU64>>,
) -> Result<AdaptiveScanOutcome, QuarryError> {
    let worker_count = requested_workers.min(stripes.len()).max(1);
    let executor = JobExecutor::new(engine_pool(), JobPriority::Foreground, worker_count)?;
    let actual_workers = executor.effective_workers().min(stripes.len()).max(1);
    shared
        .effective_workers
        .fetch_max(actual_workers, Ordering::AcqRel);

    let stripes = Arc::new(stripes);
    let next_stripe = Arc::new(AtomicUsize::new(0));
    let earliest_error_row = Arc::new(AtomicU64::new(u64::MAX));
    let collector = Arc::new(Mutex::new(ParallelCollector::new()));
    let worker_spill_cap =
        (job_budget.limit().min(EXTERNAL_SPILL_TOTAL_BYTES) / actual_workers).max(1);

    for _ in 0..actual_workers {
        let source = Arc::clone(&source);
        let header_renames = Arc::clone(&header_renames);
        let cell_edits = Arc::clone(&cell_edits);
        let order = order.clone();
        let workspace = Arc::clone(&workspace);
        let shared_worker = Arc::clone(&shared);
        let stripes = Arc::clone(&stripes);
        let next_stripe = Arc::clone(&next_stripe);
        let earliest_error_row = Arc::clone(&earliest_error_row);
        let collector = Arc::clone(&collector);
        let job_budget = Arc::clone(&job_budget);
        let large_record_gate = Arc::clone(&large_record_gate);
        let early_duplicates = early_duplicates.clone();

        let spawned = executor.spawn(&shared.cancel_requested, move || {
            let shared = shared_worker;
            let direction = order.direction();
            let dedup = order.dedup_on_flush();
            let composite_key = order.is_composite_key();
            let key_overhead = order.key_overhead_bytes();
            let mut chunk = vec![0_u8; config.encoding.chunk_size(config.chunk_bytes)];
            let mut record = Vec::new();
            let mut arena = RunArena::new();
            let mut budget_guard: Option<JobBudgetGuard> = None;
            let mut large_guard: Option<LargeRecordGuard> = None;

            let mut local_header: Option<Vec<u8>> = None;
            let mut local_first_ending: Option<(u64, Vec<u8>)> = None;
            let mut local_runs: Vec<SpillFile> = Vec::new();
            let mut local_rows = 0_u64;
            let mut local_physical_rows = 0_u64;
            let mut local_max_key_bytes = 0_usize;
            let mut local_maximum_width = 0_usize;
            let mut local_records = RecordMultisetFingerprint::default();

            let flush_arena_and_collected = |arena: &mut RunArena,
                                             budget_guard: &mut Option<JobBudgetGuard>,
                                             local_runs: &mut Vec<SpillFile>,
                                             retain_bytes_cap: usize|
             -> Result<bool, QuarryError> {
                job_budget.mark_spilled();
                if !arena.is_empty() {
                    let Some((run, dropped)) = arena.flush_to_workspace_mode(
                        direction,
                        dedup,
                        &workspace,
                        &shared,
                        retain_bytes_cap,
                    )?
                    else {
                        return Ok(false);
                    };
                    record_flushed_run(run, dropped, early_duplicates.as_deref(), local_runs);
                    sync_budget_after_flush(arena, budget_guard);
                } else if retain_bytes_cap == 0 {
                    arena.clear_for_reuse(0);
                    budget_guard.take();
                }
                if !spill_collected_arenas(
                    &collector,
                    direction,
                    dedup,
                    early_duplicates.as_deref(),
                    &workspace,
                    &shared,
                )? {
                    return Ok(false);
                }
                Ok(true)
            };

            'worker: loop {
                if shared.cancel_requested.load(Ordering::Acquire) {
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

                advise_readahead(
                    &source,
                    stripe.start_offset,
                    stripe.end_offset.saturating_sub(stripe.start_offset),
                );
                let mut scanner = match RecordScanner::new(delimiter, config.encoding) {
                    Ok(s) => s,
                    Err(err) => {
                        collector.lock().unwrap().record_error(
                            stripe.start_row,
                            err,
                            &earliest_error_row,
                        );
                        break 'worker;
                    }
                };
                clear_record_buffer(&mut record);
                let mut pos = stripe.start_offset;
                let mut physical_row = stripe.start_row;

                let mut process_one = |raw_record: &[u8],
                                       row_num: u64,
                                       arena: &mut RunArena,
                                       budget_guard: &mut Option<JobBudgetGuard>,
                                       large_guard: &mut Option<LargeRecordGuard>,
                                       local_runs: &mut Vec<SpillFile>,
                                       local_rows: &mut u64|
                 -> Result<bool, QuarryError> {
                    if row_num >= earliest_error_row.load(Ordering::Acquire)
                        || shared.cancel_requested.load(Ordering::Acquire)
                    {
                        return Ok(false);
                    }
                    let pre_bound = conservative_record_expansion_bound(
                        raw_record,
                        delimiter,
                        row_edit_bytes(&cell_edits, &header_renames, has_header, row_num),
                        config.encoding,
                        composite_key,
                    )
                    .saturating_add(key_overhead);
                    if (pre_bound > LARGE_RECORD_GATE_BYTES || pre_bound > job_budget.limit())
                        && large_guard.is_none()
                    {
                        if let Some(g) = large_record_gate.try_acquire() {
                            *large_guard = Some(g);
                        } else {
                            if !flush_arena_and_collected(arena, budget_guard, local_runs, 0)? {
                                return Ok(false);
                            }
                            let Some(g) = large_record_gate.acquire(&shared.cancel_requested)
                            else {
                                return Ok(false);
                            };
                            *large_guard = Some(g);
                        }
                    }
                    let evaluated = evaluate_record(
                        raw_record,
                        row_num,
                        delimiter,
                        has_header,
                        &header_renames,
                        &cell_edits,
                        order.as_run_order(),
                        bom_present,
                        config,
                        &shared.cancel_requested,
                        &mut local_first_ending,
                        &mut local_maximum_width,
                    )?;
                    match evaluated {
                        ProcessedRecord::Cancelled => return Ok(false),
                        ProcessedRecord::Header(hdr) => {
                            local_header = Some(hdr);
                            shared.header_rows.store(1, Ordering::Release);
                        }
                        ProcessedRecord::Data {
                            key,
                            record: effective_record,
                            ordinal,
                        } => {
                            local_max_key_bytes = local_max_key_bytes.max(key.len());
                            let footprint =
                                RunArena::entry_footprint(key.len(), effective_record.len());
                            let needs_large = footprint > LARGE_RECORD_GATE_BYTES
                                || footprint > job_budget.limit();
                            if needs_large && large_guard.is_none() {
                                if let Some(g) = large_record_gate.try_acquire() {
                                    *large_guard = Some(g);
                                } else {
                                    if !flush_arena_and_collected(
                                        arena,
                                        budget_guard,
                                        local_runs,
                                        0,
                                    )? {
                                        return Ok(false);
                                    }
                                    let Some(g) =
                                        large_record_gate.acquire(&shared.cancel_requested)
                                    else {
                                        return Ok(false);
                                    };
                                    *large_guard = Some(g);
                                }
                            }
                            let mut admission_cap = if job_budget.is_spilled() {
                                worker_spill_cap
                            } else {
                                job_budget.limit()
                            };
                            if !arena.is_empty() {
                                let spilled = job_budget.is_spilled();
                                admission_cap = if spilled {
                                    worker_spill_cap
                                } else {
                                    job_budget.limit()
                                };
                                let planned = arena.planned_capacity_bytes(
                                    key.len(),
                                    effective_record.len(),
                                    Some(admission_cap),
                                );
                                let current = budget_guard.as_ref().map_or(0, JobBudgetGuard::used);
                                let delta = planned.saturating_sub(current);
                                let fits = planned <= admission_cap
                                    && budget_guard.as_mut().is_some_and(|g| {
                                        g.try_reserve_additional(
                                            delta,
                                            spilled.then_some(worker_spill_cap),
                                        )
                                    });
                                if !fits
                                    && !flush_arena_and_collected(
                                        arena,
                                        budget_guard,
                                        local_runs,
                                        worker_spill_cap,
                                    )?
                                {
                                    return Ok(false);
                                }
                            }
                            if arena.is_empty() {
                                let spilled = job_budget.is_spilled();
                                admission_cap = if spilled {
                                    worker_spill_cap
                                } else {
                                    job_budget.limit()
                                };
                                if let Some(guard) = budget_guard.as_mut() {
                                    let planned = arena.planned_capacity_bytes(
                                        key.len(),
                                        effective_record.len(),
                                        Some(admission_cap),
                                    );
                                    let delta = planned.saturating_sub(guard.used());
                                    if planned > admission_cap
                                        || !guard.try_reserve_additional(
                                            delta,
                                            spilled.then_some(worker_spill_cap),
                                        )
                                    {
                                        arena.clear_for_reuse(0);
                                        budget_guard.take();
                                    }
                                }
                                if budget_guard.is_none() {
                                    let Some((guard, admitted_spilled)) = job_budget
                                        .acquire_empty_with_spill(
                                            |is_spilled| {
                                                let cap = if is_spilled {
                                                    worker_spill_cap
                                                } else {
                                                    job_budget.limit()
                                                };
                                                arena.planned_capacity_bytes(
                                                    key.len(),
                                                    effective_record.len(),
                                                    Some(cap),
                                                )
                                            },
                                            &shared.cancel_requested,
                                            || {
                                                job_budget.mark_spilled();
                                                spill_collected_arenas(
                                                    &collector,
                                                    direction,
                                                    dedup,
                                                    early_duplicates.as_deref(),
                                                    &workspace,
                                                    &shared,
                                                )
                                            },
                                        )?
                                    else {
                                        return Ok(false);
                                    };
                                    admission_cap = if admitted_spilled {
                                        worker_spill_cap
                                    } else {
                                        job_budget.limit()
                                    };
                                    *budget_guard = Some(guard);
                                }
                            }
                            local_records.observe(&effective_record);
                            arena.push_bounded(
                                &key,
                                &effective_record,
                                ordinal,
                                Some(admission_cap),
                            );
                            *local_rows = local_rows.saturating_add(1);
                            let needs_immediate_flush = (large_guard.is_some()
                                && (job_budget.is_spilled()
                                    || arena.capacity_bytes() > job_budget.limit()))
                                || (job_budget.is_spilled()
                                    && arena.capacity_bytes() > worker_spill_cap);
                            if needs_immediate_flush
                                && !flush_arena_and_collected(arena, budget_guard, local_runs, 0)?
                            {
                                return Ok(false);
                            }
                        }
                    }
                    local_physical_rows = local_physical_rows.saturating_add(1);
                    Ok(true)
                };

                while pos < stripe.end_offset {
                    if shared.cancel_requested.load(Ordering::Acquire)
                        || physical_row >= earliest_error_row.load(Ordering::Acquire)
                    {
                        break 'worker;
                    }
                    let want = (stripe.end_offset - pos).min(chunk.len() as u64) as usize;
                    let read = match read_at_exact_or_eof(&source, pos, &mut chunk[..want]) {
                        Ok(n) => n,
                        Err(err) => {
                            collector.lock().unwrap().record_error(
                                physical_row,
                                err.into(),
                                &earliest_error_row,
                            );
                            break 'worker;
                        }
                    };
                    if read < want {
                        collector.lock().unwrap().record_error(
                            physical_row,
                            QuarryError::SourceChanged,
                            &earliest_error_row,
                        );
                        break 'worker;
                    }
                    let mut segment_start = 0_usize;
                    let mut deferred_error: Option<(u64, QuarryError)> = None;
                    let mut stopped = false;
                    let rows_before_chunk = local_rows;
                    let scan_result = scanner.scan_chunk(&chunk[..read], pos, |absolute_end| {
                        let local_end = (absolute_end - pos) as usize;
                        if deferred_error.is_none() && !stopped {
                            let slice = &chunk[segment_start..local_end];
                            if record.len().saturating_add(slice.len()) > config.max_record_bytes {
                                deferred_error = Some((
                                    physical_row,
                                    QuarryError::RecordTooLarge {
                                        limit: config.max_record_bytes,
                                    },
                                ));
                            } else {
                                record.extend_from_slice(slice);
                                match process_one(
                                    &record,
                                    physical_row,
                                    &mut arena,
                                    &mut budget_guard,
                                    &mut large_guard,
                                    &mut local_runs,
                                    &mut local_rows,
                                ) {
                                    Ok(true) => {}
                                    Ok(false) => stopped = true,
                                    Err(err) => deferred_error = Some((physical_row, err)),
                                }
                            }
                        }
                        clear_record_buffer(&mut record);
                        large_guard.take();
                        physical_row = physical_row.saturating_add(1);
                        segment_start = local_end;
                    });
                    pos = pos.saturating_add(read as u64);
                    shared
                        .bytes_scanned
                        .fetch_add(read as u64, Ordering::Relaxed);
                    let row_delta = local_rows.saturating_sub(rows_before_chunk);
                    if row_delta > 0 {
                        shared.rows_sorted.fetch_add(row_delta, Ordering::Relaxed);
                    }
                    shared.preparing.store(false, Ordering::Release);

                    if let Some((err_row, err)) = deferred_error {
                        collector
                            .lock()
                            .unwrap()
                            .record_error(err_row, err, &earliest_error_row);
                        break 'worker;
                    }
                    if stopped || shared.cancel_requested.load(Ordering::Acquire) {
                        break 'worker;
                    }
                    if let Err(err) = scan_result {
                        collector.lock().unwrap().record_error(
                            physical_row,
                            err,
                            &earliest_error_row,
                        );
                        break 'worker;
                    }
                    let remainder = &chunk[segment_start..read];
                    if record.len().saturating_add(remainder.len()) > config.max_record_bytes {
                        collector.lock().unwrap().record_error(
                            physical_row,
                            QuarryError::RecordTooLarge {
                                limit: config.max_record_bytes,
                            },
                            &earliest_error_row,
                        );
                        break 'worker;
                    }
                    if record.len().saturating_add(remainder.len()) > LARGE_RECORD_GATE_BYTES
                        && large_guard.is_none()
                    {
                        if let Some(g) = large_record_gate.try_acquire() {
                            large_guard = Some(g);
                        } else {
                            match flush_arena_and_collected(
                                &mut arena,
                                &mut budget_guard,
                                &mut local_runs,
                                0,
                            ) {
                                Ok(true) => {}
                                Ok(false) => break 'worker,
                                Err(err) => {
                                    collector.lock().unwrap().record_error(
                                        physical_row,
                                        err,
                                        &earliest_error_row,
                                    );
                                    break 'worker;
                                }
                            }
                            let Some(g) = large_record_gate.acquire(&shared.cancel_requested)
                            else {
                                break 'worker;
                            };
                            large_guard = Some(g);
                        }
                    }
                    record.extend_from_slice(remainder);
                }

                if stripe.is_eof {
                    let mut deferred_error: Option<(u64, QuarryError)> = None;
                    let mut stopped = false;
                    let rows_before_eof = local_rows;
                    let finish_result = scanner.finish(pos, |_| {
                        if deferred_error.is_none() && !stopped {
                            match process_one(
                                &record,
                                physical_row,
                                &mut arena,
                                &mut budget_guard,
                                &mut large_guard,
                                &mut local_runs,
                                &mut local_rows,
                            ) {
                                Ok(true) => {}
                                Ok(false) => stopped = true,
                                Err(err) => deferred_error = Some((physical_row, err)),
                            }
                        }
                        clear_record_buffer(&mut record);
                        large_guard.take();
                        physical_row = physical_row.saturating_add(1);
                    });
                    clear_record_buffer(&mut record);
                    large_guard.take();
                    let row_delta = local_rows.saturating_sub(rows_before_eof);
                    if row_delta > 0 {
                        shared.rows_sorted.fetch_add(row_delta, Ordering::Relaxed);
                    }
                    if let Some((err_row, err)) = deferred_error {
                        collector
                            .lock()
                            .unwrap()
                            .record_error(err_row, err, &earliest_error_row);
                        break 'worker;
                    }
                    if stopped {
                        break 'worker;
                    }
                    if let Err(err) = finish_result {
                        collector.lock().unwrap().record_error(
                            physical_row,
                            err,
                            &earliest_error_row,
                        );
                        break 'worker;
                    }
                } else if !record.is_empty()
                    || stripe
                        .end_row
                        .is_some_and(|expected| physical_row != expected)
                {
                    collector.lock().unwrap().record_error(
                        physical_row,
                        QuarryError::SourceChanged,
                        &earliest_error_row,
                    );
                    break 'worker;
                }
            }

            // Finish worker: release any oversized scratch buffer before releasing the gate,
            // sort in-memory arena in parallel, then either retain (zero-spill path)
            // or flush to disk if any worker has spilled.
            clear_record_buffer(&mut record);
            large_guard.take();
            if !shared.cancel_requested.load(Ordering::Acquire)
                && earliest_error_row.load(Ordering::Acquire) == u64::MAX
                && !arena.is_empty()
            {
                arena.sort(direction);
                let flush_final =
                    |arena: &mut RunArena,
                     budget_guard: &mut Option<JobBudgetGuard>,
                     local_runs: &mut Vec<SpillFile>| {
                        match arena
                            .flush_to_workspace_mode(direction, dedup, &workspace, &shared, 0)
                        {
                            Ok(Some((run, dropped))) => {
                                record_flushed_run(
                                    run,
                                    dropped,
                                    early_duplicates.as_deref(),
                                    local_runs,
                                );
                            }
                            Ok(None) => {}
                            Err(err) => {
                                collector.lock().unwrap().record_error(
                                    u64::MAX - 1,
                                    err,
                                    &earliest_error_row,
                                );
                            }
                        }
                        budget_guard.take();
                    };
                if job_budget.is_spilled() {
                    flush_final(&mut arena, &mut budget_guard, &mut local_runs);
                } else {
                    let mut guard = collector.lock().unwrap();
                    if job_budget.is_spilled() {
                        drop(guard);
                        flush_final(&mut arena, &mut budget_guard, &mut local_runs);
                    } else {
                        if let Some(bg) = budget_guard.as_mut() {
                            bg.shrink_used_to(arena.capacity_bytes());
                        }
                        guard.completed_arenas.push(InMemoryRun {
                            arena,
                            _budget_guard: budget_guard.take(),
                        });
                    }
                }
            } else {
                arena.clear_for_reuse(0);
                budget_guard.take();
            }

            let mut guard = collector.lock().unwrap();
            if let Some(hdr) = local_header {
                guard.header = Some(hdr);
            }
            if let Some((row, ending)) = local_first_ending
                && guard
                    .first_ending
                    .as_ref()
                    .is_none_or(|(best_row, _)| row < *best_row)
            {
                guard.first_ending = Some((row, ending));
            }
            guard.spilled_runs.extend(local_runs);
            guard.rows = guard.rows.saturating_add(local_rows);
            guard.physical_rows = guard.physical_rows.saturating_add(local_physical_rows);
            guard.max_key_bytes = guard.max_key_bytes.max(local_max_key_bytes);
            guard.maximum_width = guard.maximum_width.max(local_maximum_width);
            guard.records.merge(local_records);
        });
        if !spawned {
            break;
        }
    }

    executor.drain()?;

    if shared.cancel_requested.load(Ordering::Acquire) {
        return Ok(AdaptiveScanOutcome::Cancelled);
    }

    let mut collected = Arc::try_unwrap(collector)
        .ok()
        .expect("all parallel sort workers drained")
        .into_inner()
        .unwrap();

    if let Some((_, error)) = collected.earliest_error.take() {
        return Err(error);
    }
    if has_header && collected.header.is_none() {
        return Err(QuarryError::InvalidOption(
            "source does not contain a header row",
        ));
    }
    if cell_edits
        .last_key_value()
        .is_some_and(|((row, _), _)| *row >= collected.physical_rows)
    {
        return Err(QuarryError::InvalidOption("cell edit row is out of range"));
    }

    // If any spill happened, flush any remaining in-memory arenas so all runs go through external merge.
    if job_budget.is_spilled() || !collected.spilled_runs.is_empty() {
        let direction = order.direction();
        let dedup = order.dedup_on_flush();
        for mut item in collected.completed_arenas.drain(..) {
            if shared.cancel_requested.load(Ordering::Acquire) {
                return Ok(AdaptiveScanOutcome::Cancelled);
            }
            if let Some((run, dropped)) = item
                .arena
                .flush_to_workspace_mode(direction, dedup, &workspace, &shared, 0)?
            {
                record_flushed_run(
                    run,
                    dropped,
                    early_duplicates.as_deref(),
                    &mut collected.spilled_runs,
                );
            }
            drop(item);
        }
    }

    Ok(AdaptiveScanOutcome::Complete(AdaptiveScanSummary {
        header: collected.header,
        preferred_ending: collected
            .first_ending
            .map_or_else(|| b"\n".to_vec(), |(_, ending)| ending),
        runs: collected.spilled_runs,
        in_memory_runs: collected.completed_arenas,
        rows: collected.rows,
        max_key_bytes: collected.max_key_bytes,
        maximum_width: collected.maximum_width,
        records: collected.records,
        effective_workers: actual_workers,
    }))
}

/// Stream one or more already-sorted in-memory arenas directly into `ExportTarget` through the
/// exact same `VerifiedOutputWriter` used by external `merge_runs_to_output`.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_in_memory_runs_to_output(
    runs: &[InMemoryRun],
    output: &mut ExportTarget,
    direction: SortDirection,
    preferred_ending: &[u8],
    expected_records: RecordMultisetFingerprint,
    max_record_bytes: usize,
    prior_bytes_written: u64,
    first_record_prefix_bytes: u64,
    shared: &SharedState,
    output_index: Option<&mut StructuralIndex>,
) -> Result<Option<(u64, u64, SortVerification)>, QuarryError> {
    let mut writer = VerifiedOutputWriter::new(
        output,
        preferred_ending,
        expected_records,
        max_record_bytes,
        prior_bytes_written,
        first_record_prefix_bytes,
        shared,
        output_index,
    );

    if runs.is_empty() {
        return Ok(Some(writer.finish()?));
    }

    if runs.len() == 1 {
        let arena = &runs[0].arena;
        for idx in 0..arena.len() {
            if (idx & 0x3ff) == 0 && shared.cancel_requested.load(Ordering::Acquire) {
                return Ok(None);
            }
            let key = arena.key_at(idx);
            let ordinal = arena.ordinal_at(idx);
            writer.flush_previous_before(key, ordinal)?;
            writer.set_pending(key.to_vec(), arena.record_at(idx).to_vec(), ordinal);
        }
        return Ok(Some(writer.finish()?));
    }

    struct InMemHeapItem<'a> {
        run_index: usize,
        entry_index: usize,
        key: &'a [u8],
        ordinal: u64,
        direction: SortDirection,
    }

    impl Eq for InMemHeapItem<'_> {}
    impl PartialEq for InMemHeapItem<'_> {
        fn eq(&self, other: &Self) -> bool {
            self.run_index == other.run_index && self.entry_index == other.entry_index
        }
    }
    impl Ord for InMemHeapItem<'_> {
        fn cmp(&self, other: &Self) -> CmpOrdering {
            compare_key_ordinal(
                self.key,
                self.ordinal,
                other.key,
                other.ordinal,
                self.direction,
            )
            .reverse()
            .then_with(|| other.run_index.cmp(&self.run_index))
        }
    }
    impl PartialOrd for InMemHeapItem<'_> {
        fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
            Some(self.cmp(other))
        }
    }

    let mut heap = BinaryHeap::with_capacity(runs.len());
    for (run_index, run) in runs.iter().enumerate() {
        if !run.arena.is_empty() {
            heap.push(InMemHeapItem {
                run_index,
                entry_index: 0,
                key: run.arena.key_at(0),
                ordinal: run.arena.ordinal_at(0),
                direction,
            });
        }
    }

    let mut emitted = 0_usize;
    while let Some(item) = heap.pop() {
        if (emitted & 0x3ff) == 0 && shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        writer.flush_previous_before(item.key, item.ordinal)?;
        let arena = &runs[item.run_index].arena;
        writer.set_pending(
            item.key.to_vec(),
            arena.record_at(item.entry_index).to_vec(),
            item.ordinal,
        );
        emitted += 1;
        let next_idx = item.entry_index + 1;
        if next_idx < arena.len() {
            heap.push(InMemHeapItem {
                run_index: item.run_index,
                entry_index: next_idx,
                key: arena.key_at(next_idx),
                ordinal: arena.ordinal_at(next_idx),
                direction,
            });
        }
    }

    Ok(Some(writer.finish()?))
}

/// Count the exact RFC 4180 field width of one UTF-8 delimited record using SIMD `memchr`
/// when unquoted and `StructuralScanner<true>` when quotes are present.
pub(super) fn count_record_fields(record: &[u8], delimiter: u8) -> Result<usize, QuarryError> {
    if memchr::memchr(b'"', record).is_none() {
        return Ok(memchr::memchr_iter(delimiter, record)
            .count()
            .saturating_add(1));
    }
    let mut scanner = quarry_delimited::StructuralScanner::<true>::new(delimiter)?;
    scanner.scan_chunk(record, 0, |_| {})?;
    scanner.finish(record.len() as u64, |_| {})?;
    Ok(scanner.maximum_width())
}

/// Deduplicate one or more already-sorted in-memory arenas (ascending by `(key, ordinal)`)
/// keeping only the lowest-ordinal occurrence of each key, restore original source order
/// (in-place inside `arena.entries` for a single arena with zero extra allocation, or via
/// budgeted `(ordinal, run_index, entry_index)` triples across multiple arenas), and stream
/// directly through `VerifiedOutputWriter` with zero temporary spill files.
#[allow(clippy::too_many_arguments)]
pub(super) fn deduplicate_in_memory_runs_to_output(
    runs: &mut [InMemoryRun],
    delimiter: u8,
    duplicate_rows_counter: &AtomicU64,
    retained_rows_counter: &AtomicU64,
    output: &mut ExportTarget,
    preferred_ending: &[u8],
    max_record_bytes: usize,
    prior_bytes_written: u64,
    first_record_prefix_bytes: u64,
    shared: &SharedState,
    output_index: Option<&mut StructuralIndex>,
) -> Result<Option<(u64, u64, usize, SortVerification)>, QuarryError> {
    let mut expected_records = RecordMultisetFingerprint::default();
    let mut duplicates = 0_u64;
    let mut retained = 0_u64;
    let mut retained_max_width = 0_usize;

    if runs.len() == 1 {
        let arena = &mut runs[0].arena;
        let mut write_idx = 0_usize;
        let mut prev_key: Option<(usize, usize)> = None;
        for read_idx in 0..arena.entries.len() {
            if (read_idx & 0x3ff) == 0 && shared.cancel_requested.load(Ordering::Acquire) {
                return Ok(None);
            }
            let entry = arena.entries[read_idx];
            let key = &arena.bytes[entry.key_offset..entry.key_offset + entry.key_len];
            if prev_key.is_some_and(|(off, len)| &arena.bytes[off..off + len] == key) {
                duplicates = duplicates.saturating_add(1);
            } else {
                prev_key = Some((entry.key_offset, entry.key_len));
                let rec = &arena.bytes[entry.record_offset..entry.record_offset + entry.record_len];
                expected_records.observe(rec);
                retained_max_width = retained_max_width.max(count_record_fields(rec, delimiter)?);
                arena.entries[write_idx] = entry;
                write_idx += 1;
                retained = retained.saturating_add(1);
            }
        }
        arena.entries.truncate(write_idx);
        arena.sorted = false;

        duplicate_rows_counter.fetch_add(duplicates, Ordering::AcqRel);
        retained_rows_counter.fetch_add(retained, Ordering::AcqRel);

        arena.entries.sort_unstable_by_key(|entry| entry.ordinal);

        let mut writer = VerifiedOutputWriter::new(
            output,
            preferred_ending,
            expected_records,
            max_record_bytes,
            prior_bytes_written,
            first_record_prefix_bytes,
            shared,
            output_index,
        );
        for (idx, entry) in arena.entries.iter().enumerate() {
            if (idx & 0x3ff) == 0 && shared.cancel_requested.load(Ordering::Acquire) {
                return Ok(None);
            }
            let key_bytes = entry.ordinal.to_be_bytes();
            writer.flush_previous_before(&key_bytes, entry.ordinal)?;
            let record =
                arena.bytes[entry.record_offset..entry.record_offset + entry.record_len].to_vec();
            writer.set_pending(key_bytes.to_vec(), record, entry.ordinal);
        }

        let (rows_written, bytes_written, verification) = writer.finish()?;
        return Ok(Some((
            rows_written,
            bytes_written,
            retained_max_width,
            verification,
        )));
    }

    let total_entries: usize = runs.iter().map(|run| run.arena.len()).sum();
    let mut survivors: Vec<(u64, usize, usize)> = Vec::with_capacity(total_entries);

    if runs.len() > 1 {
        struct DedupHeapItem<'a> {
            run_index: usize,
            entry_index: usize,
            key: &'a [u8],
            ordinal: u64,
        }
        impl Eq for DedupHeapItem<'_> {}
        impl PartialEq for DedupHeapItem<'_> {
            fn eq(&self, other: &Self) -> bool {
                self.run_index == other.run_index && self.entry_index == other.entry_index
            }
        }
        impl Ord for DedupHeapItem<'_> {
            fn cmp(&self, other: &Self) -> CmpOrdering {
                compare_key_ordinal(
                    self.key,
                    self.ordinal,
                    other.key,
                    other.ordinal,
                    SortDirection::Ascending,
                )
                .reverse()
                .then_with(|| other.run_index.cmp(&self.run_index))
            }
        }
        impl PartialOrd for DedupHeapItem<'_> {
            fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
                Some(self.cmp(other))
            }
        }

        let mut heap = BinaryHeap::with_capacity(runs.len());
        for (run_index, run) in runs.iter().enumerate() {
            if !run.arena.is_empty() {
                heap.push(DedupHeapItem {
                    run_index,
                    entry_index: 0,
                    key: run.arena.key_at(0),
                    ordinal: run.arena.ordinal_at(0),
                });
            }
        }

        let mut prev: Option<(usize, usize)> = None;
        let mut step = 0_usize;
        while let Some(item) = heap.pop() {
            if (step & 0x3ff) == 0 && shared.cancel_requested.load(Ordering::Acquire) {
                return Ok(None);
            }
            step = step.saturating_add(1);
            let arena = &runs[item.run_index].arena;
            if prev.is_some_and(|(pr, pe)| runs[pr].arena.key_at(pe) == item.key) {
                duplicates = duplicates.saturating_add(1);
            } else {
                prev = Some((item.run_index, item.entry_index));
                let rec = arena.record_at(item.entry_index);
                expected_records.observe(rec);
                retained_max_width = retained_max_width.max(count_record_fields(rec, delimiter)?);
                survivors.push((item.ordinal, item.run_index, item.entry_index));
                retained = retained.saturating_add(1);
            }
            let next_idx = item.entry_index + 1;
            if next_idx < arena.len() {
                heap.push(DedupHeapItem {
                    run_index: item.run_index,
                    entry_index: next_idx,
                    key: arena.key_at(next_idx),
                    ordinal: arena.ordinal_at(next_idx),
                });
            }
        }
    }

    duplicate_rows_counter.fetch_add(duplicates, Ordering::AcqRel);
    retained_rows_counter.fetch_add(retained, Ordering::AcqRel);

    survivors.sort_unstable_by_key(|&(ordinal, _, _)| ordinal);

    let mut writer = VerifiedOutputWriter::new(
        output,
        preferred_ending,
        expected_records,
        max_record_bytes,
        prior_bytes_written,
        first_record_prefix_bytes,
        shared,
        output_index,
    );
    for (idx, (ordinal, run_index, entry_index)) in survivors.into_iter().enumerate() {
        if (idx & 0x3ff) == 0 && shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        let key_bytes = ordinal.to_be_bytes();
        writer.flush_previous_before(&key_bytes, ordinal)?;
        let record = runs[run_index].arena.record_at(entry_index).to_vec();
        writer.set_pending(key_bytes.to_vec(), record, ordinal);
    }

    let (rows_written, bytes_written, verification) = writer.finish()?;
    Ok(Some((
        rows_written,
        bytes_written,
        retained_max_width,
        verification,
    )))
}

/// Compute the safe concurrent merge group count that splits `MERGE_MEMORY_BUDGET_BYTES`
/// and stays strictly inside `fd_budget_limit`, scaling down to 1 when required.
pub(super) fn compute_merge_concurrency(
    requested_workers: usize,
    group_count: usize,
    max_group_len: usize,
    max_key_bytes: usize,
    max_record_bytes: usize,
    fd_budget_limit: usize,
) -> usize {
    let max_by_work = requested_workers.min(group_count).max(1);
    if max_by_work <= 1 {
        return 1;
    }
    let key_bytes = max_key_bytes.max(1);
    let group_mem = max_record_bytes
        .saturating_add(key_bytes.saturating_mul(max_group_len.saturating_add(1)))
        .saturating_add(
            max_group_len
                .saturating_add(1)
                .saturating_mul(MERGE_STREAM_BUFFER_BYTES),
        );
    let max_by_mem = if group_mem == 0 {
        max_by_work
    } else {
        (MERGE_MEMORY_BUDGET_BYTES / group_mem).max(1)
    };
    let fds_per_group = max_group_len.saturating_add(1).max(1);
    let max_by_fd = (fd_budget_limit / fds_per_group).max(1);
    max_by_work.min(max_by_mem).min(max_by_fd).max(1)
}

/// Reduce runs in parallel across disjoint merge groups, splitting the 256 MiB merge budget,
/// reserving FDs via `fd_budget`, and unlinking each group's input runs immediately on completion.
#[allow(clippy::too_many_arguments)]
pub(super) fn reduce_runs_parallel(
    runs: &mut Vec<SpillFile>,
    direction: SortDirection,
    max_key_bytes: usize,
    config: SortConfig,
    workspace: &Arc<LazyRunWorkspace>,
    shared: &Arc<SharedState>,
    requested_workers: usize,
    fd_budget: &Arc<FdBudget>,
) -> Result<bool, QuarryError> {
    reduce_runs_parallel_with_dedup(
        runs,
        direction,
        max_key_bytes,
        config,
        workspace,
        shared,
        requested_workers,
        fd_budget,
        None,
    )
}

/// Reduce runs in parallel, optionally eliminating duplicate keys (ascending order) during each
/// intermediate merge pass and recording dropped duplicates in `dedup_counter`.
#[allow(clippy::too_many_arguments)]
pub(super) fn reduce_runs_parallel_with_dedup(
    runs: &mut Vec<SpillFile>,
    direction: SortDirection,
    max_key_bytes: usize,
    config: SortConfig,
    workspace: &Arc<LazyRunWorkspace>,
    shared: &Arc<SharedState>,
    requested_workers: usize,
    fd_budget: &Arc<FdBudget>,
    dedup_counter: Option<Arc<AtomicU64>>,
) -> Result<bool, QuarryError> {
    let merge_fan_in = effective_merge_fan_in(config, max_key_bytes);
    while runs.len() > merge_fan_in {
        let mut groups: Vec<Vec<SpillFile>> = Vec::with_capacity(runs.len().div_ceil(merge_fan_in));
        let mut drained = runs.drain(..);
        loop {
            let chunk: Vec<SpillFile> = drained.by_ref().take(merge_fan_in).collect();
            if chunk.is_empty() {
                break;
            }
            groups.push(chunk);
        }
        drop(drained);
        let group_count = groups.len();
        let max_group_len = groups.iter().map(Vec::len).max().unwrap_or(1);
        let concurrency = compute_merge_concurrency(
            requested_workers,
            group_count,
            max_group_len,
            max_key_bytes,
            config.max_record_bytes,
            fd_budget.limit(),
        );

        if concurrency <= 1 {
            let mut next = Vec::with_capacity(group_count);
            for group in groups {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    return Ok(false);
                }
                let Some(fd_guard) =
                    fd_budget.acquire(group.len().saturating_add(1), &shared.cancel_requested)
                else {
                    return Ok(false);
                };
                let (path, writer) = workspace.create_run()?;
                let merged = merge_runs_to_run_with_dedup(
                    &group,
                    writer,
                    direction,
                    config.max_record_bytes,
                    shared,
                    dedup_counter.as_deref(),
                )?;
                drop(fd_guard);
                if !merged {
                    return Ok(false);
                }
                for old in &group {
                    let bytes = old.metadata()?.len();
                    old.remove()?;
                    shared.remove_temporary_bytes(bytes);
                }
                next.push(path);
                shared.runs_created.fetch_add(1, Ordering::AcqRel);
            }
            *runs = next;
            shared.merge_passes.fetch_add(1, Ordering::AcqRel);
            continue;
        }

        let executor = JobExecutor::new(engine_pool(), JobPriority::Foreground, concurrency)?;
        shared
            .effective_workers
            .fetch_max(executor.effective_workers(), Ordering::AcqRel);
        let outputs: Arc<Mutex<Vec<Option<SpillFile>>>> =
            Arc::new(Mutex::new(vec![None; group_count]));
        let first_error: Arc<Mutex<Option<QuarryError>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));

        for (group_index, group) in groups.into_iter().enumerate() {
            if shared.cancel_requested.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
                break;
            }
            // Acquire FD reservation on the coordinator thread BEFORE spawning onto `EnginePool`
            // so merge worker threads never block waiting on cross-job `FdBudget`.
            let Some(fd_guard) =
                fd_budget.acquire(group.len().saturating_add(1), &shared.cancel_requested)
            else {
                break;
            };
            let workspace = Arc::clone(workspace);
            let shared_worker = Arc::clone(shared);
            let outputs = Arc::clone(&outputs);
            let first_error = Arc::clone(&first_error);
            let stop = Arc::clone(&stop);
            let dedup_counter = dedup_counter.clone();

            let spawned = executor.spawn(&shared.cancel_requested, move || {
                let _fd_guard = fd_guard;
                let shared = shared_worker;
                if shared.cancel_requested.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
                    return;
                }
                let outcome = (|| -> Result<Option<SpillFile>, QuarryError> {
                    let (path, writer) = workspace.create_run()?;
                    let merged = merge_runs_to_run_with_dedup(
                        &group,
                        writer,
                        direction,
                        config.max_record_bytes,
                        &shared,
                        dedup_counter.as_deref(),
                    )?;
                    if !merged {
                        return Ok(None);
                    }
                    // Unlink this group's input runs immediately on completion so peak scratch
                    // remains strictly bounded to two generations.
                    for old in &group {
                        let bytes = old.metadata()?.len();
                        old.remove()?;
                        shared.remove_temporary_bytes(bytes);
                    }
                    shared.runs_created.fetch_add(1, Ordering::AcqRel);
                    Ok(Some(path))
                })();
                drop(_fd_guard);
                match outcome {
                    Ok(Some(path)) => {
                        outputs.lock().unwrap()[group_index] = Some(path);
                    }
                    Ok(None) => {
                        stop.store(true, Ordering::Release);
                    }
                    Err(error) => {
                        stop.store(true, Ordering::Release);
                        let mut err_slot = first_error.lock().unwrap();
                        if err_slot.is_none() {
                            *err_slot = Some(error);
                        }
                    }
                }
            });
            if !spawned {
                break;
            }
        }

        executor.drain()?;
        if let Some(error) = first_error.lock().unwrap().take() {
            return Err(error);
        }
        if shared.cancel_requested.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
            return Ok(false);
        }
        let next: Vec<SpillFile> = outputs
            .lock()
            .unwrap()
            .drain(..)
            .map(|item| item.expect("every merge group produced a run"))
            .collect();
        *runs = next;
        shared.merge_passes.fetch_add(1, Ordering::AcqRel);
    }

    Ok(true)
}

#[allow(dead_code)]
pub(super) fn write_output_header_and_bom(
    output: &mut ExportTarget,
    bom_present: bool,
    header: Option<&[u8]>,
    index: &mut StructuralIndex,
    shared: &SharedState,
) -> Result<u64, QuarryError> {
    let mut bytes_written = 0_u64;
    if bom_present {
        write_output(output, UTF8_BOM, shared)?;
        bytes_written = output.bytes_written();
    }
    if let Some(header) = header {
        write_output(output, header, shared)?;
        bytes_written = output.bytes_written();
        if bytes_written > output.record_limit() as u64 {
            return Err(QuarryError::RecordTooLarge {
                limit: output.record_limit(),
            });
        }
        index.record_boundary(bytes_written);
    }
    shared.bytes_written.store(bytes_written, Ordering::Release);
    Ok(bytes_written)
}
