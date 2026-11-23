//! Partitioned multi-threaded hash-join with automatic ADR 0011 sort-merge skew fallback.
//!
//! Seekable inputs are partitioned once during `analyze_join` into compact key + 24-byte
//! `RowLocator` frames inside the owned `SpillWorkspace`. Empty/missing keys (`key[0] == 0`)
//! bypass hashing into ordinal-sorted unmatchable streams. Non-empty partitions are analyzed
//! and probed in parallel on `EnginePool` under a shared tracked byte budget; any partition
//! whose hash table would exceed its memory cap or exhibits heavy many-to-many skew falls back
//! automatically to ADR 0011 external sort-merge and bounded duplicate blocks.

use std::collections::{BTreeMap, BinaryHeap};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::*;
use crate::pool::{JobPriority, default_worker_threads, engine_pool};

const MIN_HASH_RUN_BYTES: usize = 64 * 1024;
const MIN_HASH_GROUP_BYTES: usize = 64 * 1024;
const MIN_HASH_MEMORY_BYTES: usize = 2 * 1024 * 1024;
const MIN_PARTITION_MEM_CAP: usize = 64 * 1024;
const MAX_PARTITION_MEM_CAP: usize = 128 * 1024 * 1024;
const MAX_HASH_WORKERS: usize = 8;
/// Maximum raw framed-key bytes retained per sample ordering key so sample maps remain bounded
/// regardless of source key length (well within `GROUP_EXAMPLE_RESERVE`).
const MAX_SAMPLE_KEY_PREFIX: usize = 256;
/// Many-to-many pair threshold above which a partition falls back to bounded-block sort-merge
/// during creation so explosive skew never retains unbounded groups in hash buffers.
const SKEW_FALLBACK_PAIR_THRESHOLD: u64 = 65_536;
const EMPTY_SLOT: u32 = 0;
const NO_NEXT_ROW: u32 = u32::MAX;
const MATCHED_BIT: u32 = 1_u32 << 31;
const KEY_LEN_MASK: u32 = !MATCHED_BIT;

/// 128-bit dual-stream digest of a framed comparison key (zero heap allocation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct KeyDigest {
    a: u64,
    b: u64,
}

impl KeyDigest {
    const INIT_A: u64 = 0xcbf2_9ce4_8422_2325;
    const INIT_B: u64 = 0x9e37_79b9_7f4a_7c15;

    #[inline]
    fn new() -> Self {
        Self {
            a: Self::INIT_A,
            b: Self::INIT_B,
        }
    }

    #[inline]
    fn feed_byte(&mut self, byte: u8) {
        self.a ^= u64::from(byte);
        self.a = self.a.wrapping_mul(0x0000_0100_0000_01b3);
        self.b ^= u64::from(byte).wrapping_add(0x51);
        self.b = self.b.wrapping_mul(0x9e37_79b1_85eb_ca87).rotate_left(7);
    }

    #[inline]
    fn feed_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.feed_byte(byte);
        }
    }

    #[inline]
    fn finish(mut self) -> Self {
        self.a ^= self.a >> 30;
        self.a = self.a.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        self.a ^= self.a >> 27;
        self.a = self.a.wrapping_mul(0x94d0_49bb_1331_11eb);
        self.a ^= self.a >> 31;

        self.b ^= self.b >> 33;
        self.b = self.b.wrapping_mul(0xff51_afd7_ed55_8ccd);
        self.b ^= self.b >> 33;
        self.b = self.b.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        self.b ^= self.b >> 33;
        self
    }

    fn from_bytes(key: &[u8]) -> Self {
        let mut h = Self::new();
        h.feed_bytes(key);
        h.finish()
    }

    fn from_empty_ordinal(ordinal: u64) -> Self {
        let mut h = Self::new();
        h.feed_byte(0);
        h.feed_bytes(&ordinal.to_be_bytes());
        h.finish()
    }

    fn from_fields(
        fields: &[Cow<'_, [u8]>],
        columns: &[usize],
        ordinal: u64,
        case: JoinCase,
    ) -> Self {
        if columns
            .iter()
            .any(|&column| fields.get(column).is_none_or(|field| field.is_empty()))
        {
            return Self::from_empty_ordinal(ordinal);
        }
        let mut h = Self::new();
        h.feed_byte(1);
        for &column in columns {
            let value = &fields[column];
            h.feed_bytes(&(value.len() as u64).to_be_bytes());
            for &byte in value.as_ref() {
                h.feed_byte(case.byte(byte));
            }
        }
        h.finish()
    }
}

/// Bounded sample ordering key (`<= 256` prefix bytes + 16-byte digest) so duplicate and
/// unmatched sample maps retain O(1) bytes even for multi-megabyte join keys.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SampleKey {
    prefix: Vec<u8>,
    digest: KeyDigest,
}

impl SampleKey {
    fn from_bytes(key: &[u8]) -> Self {
        let len = key.len().min(MAX_SAMPLE_KEY_PREFIX);
        let mut prefix = Vec::with_capacity(len);
        prefix.extend_from_slice(&key[..len]);
        Self {
            prefix,
            digest: KeyDigest::from_bytes(key),
        }
    }

    fn cmp_raw(&self, key: &[u8]) -> CmpOrdering {
        let prefix = &key[..key.len().min(MAX_SAMPLE_KEY_PREFIX)];
        match self.prefix.as_slice().cmp(prefix) {
            CmpOrdering::Equal => self.digest.cmp(&KeyDigest::from_bytes(key)),
            other => other,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct PartitionMeta {
    pub(super) file: Option<SpillFile>,
    pub(super) rows: u64,
    pub(super) bytes: u64,
    pub(super) key_bytes: u64,
    pub(super) max_key_bytes: usize,
    pub(super) fingerprint: LocatorFingerprint,
}

#[derive(Debug, Default)]
pub(super) struct EmptyPartitionMeta {
    pub(super) file: Option<SpillFile>,
    pub(super) rows: u64,
    pub(super) bytes: u64,
    pub(super) value_bytes: u64,
    pub(super) max_value_bytes: usize,
    pub(super) fingerprint: LocatorFingerprint,
    pub(super) pair_fingerprint: LocatorFingerprint,
    /// First `max(MAX_GROUP_EXAMPLES, config.preview_rows)` empty-key rows in source order.
    pub(super) first_rows: Vec<RowRef>,
}

#[derive(Debug)]
pub(super) struct PartitionedInput {
    pub(super) partitions: Vec<PartitionMeta>,
    pub(super) empty: EmptyPartitionMeta,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PartitionPlan {
    pub(super) build_side: usize,
    pub(super) use_sort_merge: bool,
    pub(super) max_group_pairs: u64,
    pub(super) fallback_extra_bytes: u64,
}

#[derive(Debug)]
pub(super) struct HashJoinPlan {
    pub(super) partition_count: usize,
    pub(super) sides: [PartitionedInput; 2],
    pub(super) partition_plans: Vec<PartitionPlan>,
    pub(super) max_fallback_extra_bytes: u64,
}

pub(super) fn should_use_hash_join(inputs: &[&Inspected; 2], config: JoinConfig) -> bool {
    if !inputs
        .iter()
        .all(|input| input.storage == FrameStorage::Seekable)
    {
        return false;
    }
    #[cfg(test)]
    {
        if storage_test::force_sort_merge() {
            return false;
        }
        if storage_test::force_hash_join() {
            return true;
        }
    }
    config.memory_bytes >= MIN_HASH_MEMORY_BYTES
        && config.run_memory_bytes >= MIN_HASH_RUN_BYTES
        && config.group_memory_bytes >= MIN_HASH_GROUP_BYTES
        && config.max_merge_fan_in >= 4
}

fn choose_partition_count(total_frame_bytes: u64, config: JoinConfig) -> usize {
    let fd_cap = engine_pool().fd_budget().limit();
    let max_by_fd = (fd_cap / 4).max(2);
    let max_by_fan_in = config.max_merge_fan_in.max(2);
    let max_by_run_mem = (config.run_memory_bytes / (16 * 1024)).max(2);
    let cap = 32_usize
        .min(max_by_fd)
        .min(max_by_fan_in)
        .min(max_by_run_mem);
    let desired = if total_frame_bytes < 256 * 1024 {
        4
    } else if total_frame_bytes < 4 * 1024 * 1024 {
        8
    } else if total_frame_bytes < 64 * 1024 * 1024 {
        16
    } else {
        32
    };
    let target = desired.min(cap).max(2);
    1_usize << (usize::BITS - 1 - target.leading_zeros())
}

fn hash_budget_limits(config: JoinConfig) -> (usize, usize) {
    let fixed_overhead = config
        .preview_bytes
        .saturating_add(GROUP_EXAMPLE_RESERVE)
        .saturating_add(config.group_memory_bytes)
        .saturating_add(config.run_memory_bytes)
        .saturating_add(READ_CHUNK.saturating_mul(config.max_merge_fan_in + 4));
    let available = config.memory_bytes.saturating_sub(fixed_overhead);
    let total = (available / 2).clamp(MIN_PARTITION_MEM_CAP, 256 * 1024 * 1024);
    let per_partition = (total / 2).clamp(MIN_PARTITION_MEM_CAP, MAX_PARTITION_MEM_CAP);
    #[cfg(test)]
    {
        if let Some((forced_total, forced_per_part)) = storage_test::force_hash_budget_limits() {
            return (forced_total.max(1), forced_per_part.max(1));
        }
        if let Some(forced) = storage_test::force_hash_partition_fallback_bytes() {
            return (total.min(forced.max(1)), forced.max(1));
        }
    }
    (total, per_partition)
}

#[inline]
fn hash_key(key: &[u8]) -> u64 {
    #[cfg(test)]
    if storage_test::force_hash_collision() {
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in key {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

#[inline]
fn partition_for_hash(h: u64, partition_count: usize) -> usize {
    debug_assert!(partition_count.is_power_of_two());
    ((h >> 32) as usize) & (partition_count - 1)
}

/// Job-scoped tracked byte budget with cancellation-aware wait under backpressure.
struct JobByteBudget {
    total: usize,
    used: Mutex<usize>,
    available: Condvar,
}

struct JobBytePermit<'a> {
    budget: &'a JobByteBudget,
    bytes: usize,
}

impl Drop for JobBytePermit<'_> {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let mut used = self.budget.used.lock().unwrap();
        *used = used.saturating_sub(self.bytes);
        self.budget.available.notify_all();
    }
}

impl JobByteBudget {
    fn new(total: usize) -> Self {
        Self {
            total: total.max(1),
            used: Mutex::new(0),
            available: Condvar::new(),
        }
    }

    fn acquire(&self, bytes: usize, state: &Progress) -> JResult<JobBytePermit<'_>> {
        let need = bytes.min(self.total);
        if need == 0 {
            return Ok(JobBytePermit {
                budget: self,
                bytes: 0,
            });
        }
        let mut used = self.used.lock().unwrap();
        loop {
            state.check()?;
            if used.saturating_add(need) <= self.total {
                *used += need;
                return Ok(JobBytePermit {
                    budget: self,
                    bytes: need,
                });
            }
            #[cfg(test)]
            storage_test::notify_budget_wait();
            let (next, _) = self
                .available
                .wait_timeout(used, Duration::from_millis(10))
                .unwrap();
            used = next;
        }
    }
}

/// Run up to `desired_workers` tasks on the global `EnginePool` and join all of them before returning.
fn run_on_engine_pool<F>(desired_workers: usize, task_factory: F) -> JResult<()>
where
    F: Fn(usize) + Send + Sync + 'static,
{
    let pool = engine_pool();
    let desired = desired_workers.max(1);
    #[cfg(test)]
    let desired = if storage_test::force_single_pool_worker() {
        1
    } else {
        desired
    };
    let workers = pool.ensure_workers(desired)?;
    if workers <= 1 {
        task_factory(0);
        return Ok(());
    }
    #[cfg(test)]
    let fault_hook = crate::join_faults::capture();
    #[cfg(test)]
    let storage_hook = storage_test::capture();

    let factory = Arc::new(task_factory);
    let latch = Arc::new((Mutex::new(workers), Condvar::new()));
    let panicked = Arc::new(AtomicBool::new(false));

    for worker_idx in 0..workers {
        let factory = Arc::clone(&factory);
        let latch = Arc::clone(&latch);
        let panicked = Arc::clone(&panicked);
        #[cfg(test)]
        let fault_hook = fault_hook.clone();
        #[cfg(test)]
        let storage_hook = storage_hook.clone();

        pool.submit(
            JobPriority::Foreground,
            Box::new(move || {
                struct DoneGuard<F> {
                    factory: Option<Arc<F>>,
                    latch: Arc<(Mutex<usize>, Condvar)>,
                    panicked: Arc<AtomicBool>,
                    completed: bool,
                }
                impl<F> Drop for DoneGuard<F> {
                    fn drop(&mut self) {
                        // Drop the worker's Arc<F> (and every Arc captured by the task) strictly
                        // before decrementing the latch so the coordinator can safely unwrap Arcs.
                        self.factory.take();
                        if !self.completed {
                            self.panicked.store(true, Ordering::Release);
                        }
                        let mut rem = self.latch.0.lock().unwrap();
                        *rem = rem.saturating_sub(1);
                        if *rem == 0 {
                            self.latch.1.notify_all();
                        }
                    }
                }
                let mut guard = DoneGuard {
                    factory: Some(factory),
                    latch,
                    panicked,
                    completed: false,
                };
                #[cfg(test)]
                let _fault_scope = crate::join_faults::install(fault_hook);
                #[cfg(test)]
                let _storage_scope = storage_test::install(storage_hook);
                guard.factory.as_ref().expect("factory present")(worker_idx);
                guard.completed = true;
            }),
        );
    }
    drop(factory);

    let mut rem = latch.0.lock().unwrap();
    while *rem > 0 {
        rem = latch.1.wait(rem).unwrap();
    }
    drop(rem);
    if panicked.load(Ordering::Acquire) {
        return Err(QuarryError::WorkerPanicked.into());
    }
    Ok(())
}

/// Run both logical sides (`0` and `1`) across up to 2 pool workers, falling back to running both
/// sequentially on the caller thread if the pool returns only 1 worker.
fn run_pair_on_engine_pool<F>(side_task: F) -> JResult<()>
where
    F: Fn(usize) + Send + Sync + 'static,
{
    let next_side = Arc::new(AtomicUsize::new(0));
    run_on_engine_pool(2, move |_| {
        loop {
            let side = next_side.fetch_add(1, Ordering::AcqRel);
            if side >= 2 {
                break;
            }
            side_task(side);
        }
    })
}

/// Run two independent inspection passes in parallel while preserving Main-before-Additional error
/// priority and stopping Additional promptly via a side-local signal if Main fails.
pub(super) fn inspect_pair(
    inputs: [JoinInput; 2],
    working: [Option<JoinWorkingSource>; 2],
    config: JoinConfig,
    state: &Arc<Progress>,
) -> JResult<[Inspected; 2]> {
    state.phase(JoinPhase::Inspecting);
    let [left_input, right_input] = inputs;
    let [left_working, right_working] = working;

    let main_failed = Arc::new(AtomicBool::new(false));
    let right_state = Arc::new(Progress::with_parent(Arc::clone(state)));
    let left_result: Arc<Mutex<Option<JResult<Inspected>>>> = Arc::new(Mutex::new(None));
    let right_result: Arc<Mutex<Option<JResult<Inspected>>>> = Arc::new(Mutex::new(None));

    let left_slot = Arc::new(Mutex::new(Some((left_input, left_working))));
    let right_slot = Arc::new(Mutex::new(Some((right_input, right_working))));
    let state_arc = Arc::clone(state);
    let right_state_worker = Arc::clone(&right_state);
    let main_failed_worker = Arc::clone(&main_failed);
    let left_result_worker = Arc::clone(&left_result);
    let right_result_worker = Arc::clone(&right_result);

    run_pair_on_engine_pool(move |idx| {
        if idx == 0 {
            let Some((input, working)) = left_slot.lock().unwrap().take() else {
                return;
            };
            let res = inspect_inner(input, working, config, &state_arc);
            if res.is_err() {
                main_failed_worker.store(true, Ordering::Release);
                right_state_worker.cancel.store(true, Ordering::Release);
            }
            *left_result_worker.lock().unwrap() = Some(res);
        } else {
            if main_failed_worker.load(Ordering::Acquire) {
                return;
            }
            let Some((input, working)) = right_slot.lock().unwrap().take() else {
                return;
            };
            let res = inspect_inner(input, working, config, &right_state_worker);
            *right_result_worker.lock().unwrap() = Some(res);
        }
    })?;

    state
        .bytes
        .fetch_add(right_state.bytes.load(Ordering::Acquire), Ordering::Release);

    let left = left_result
        .lock()
        .unwrap()
        .take()
        .expect("left inspect finished")?;
    let right = right_result
        .lock()
        .unwrap()
        .take()
        .expect("right inspect finished")?;
    Ok([left, right])
}

fn spill_io_error(workspace: &SpillWorkspace, state: &Progress, cause: io::Error) -> QuarryError {
    let phase = if state.phase.load(Ordering::Acquire) == JoinPhase::Creating as usize {
        JoinPhase::Creating
    } else {
        JoinPhase::Sorting
    };
    crate::storage::storage_error(
        &workspace.path,
        io::Error::new(
            cause.kind(),
            format!(
                "{}: {cause}. Free disk space or choose another temporary folder, then analyze again.",
                phase.label()
            ),
        ),
    )
}

fn write_partition_frame(
    workspace: &SpillWorkspace,
    state: &Progress,
    writer: &mut impl Write,
    key: &[u8],
    record: &[u8],
    ordinal: u64,
) -> JResult<u64> {
    #[cfg(test)]
    crate::join_faults::point(crate::join_faults::Event::SpillWrite)
        .map_err(|err| spill_io_error(workspace, state, err))?;
    spill::write_frame(writer, key, record, ordinal)
        .map_err(|err| spill_io_error(workspace, state, err).into())
}

fn flush_partition_writer(
    workspace: &SpillWorkspace,
    state: &Progress,
    writer: &mut impl Write,
) -> JResult<()> {
    #[cfg(test)]
    crate::join_faults::point(crate::join_faults::Event::SpillFlush)
        .map_err(|err| spill_io_error(workspace, state, err))?;
    writer
        .flush()
        .map_err(|err| spill_io_error(workspace, state, err).into())
}

#[allow(clippy::too_many_arguments)]
fn partition_one_input(
    input: &Inspected,
    side: usize,
    projection: &[(usize, bool)],
    partition_count: usize,
    writer_buf_bytes: usize,
    workspace: &SpillWorkspace,
    config: JoinConfig,
    state: &Progress,
) -> JResult<(PartitionedInput, LocatorFingerprint)> {
    let _fd_guard = engine_pool()
        .fd_budget()
        .acquire(partition_count.saturating_add(2), &state.cancel)
        .ok_or(Failure::Cancelled)?;

    let mut part_metas: Vec<PartitionMeta> = (0..partition_count)
        .map(|_| PartitionMeta::default())
        .collect();
    let mut part_writers: Vec<Option<BufWriter<File>>> =
        (0..partition_count).map(|_| None).collect();
    let mut empty_meta = EmptyPartitionMeta::default();
    let mut empty_writer: Option<BufWriter<File>> = None;
    let empty_sample_limit = MAX_GROUP_EXAMPLES.max(config.preview_rows);

    let mut rows = 0_u64;
    let mut total_bytes = 0_u64;
    let mut overall_fp = LocatorFingerprint::default();

    scan(
        ScanSource {
            path: &input.physical_path,
            stamp: &input.stamp,
            working: input.working.as_ref(),
        },
        input.info.delimiter,
        input.info.encoding,
        config,
        state,
        |record, row, source_offset, raw_len| {
            if input.info.has_header && row == 0 {
                return Ok(());
            }
            let parsed = fields(
                record,
                input.info.delimiter,
                false,
                &input.info.path,
                row,
                config,
            )?;
            if parsed.len() > input.info.columns.len() {
                return Err(error("Prepared row exceeds the analyzed schema.").into());
            }
            let comparison = key(&parsed, &input.key_columns, rows, config)?;
            let value_bytes =
                row_value_bytes_from_fields(projection, input.output_delimiter, &parsed)?;
            drop(parsed);
            let locator = RowLocator {
                source_offset,
                record_len: raw_len,
                value_bytes: value_bytes as u64,
            };
            overall_fp.observe_row(rows, locator, &comparison);
            let loc_bytes = locator.to_le_bytes();

            let written = if comparison[0] == 0 {
                let writer = match &mut empty_writer {
                    Some(w) => w,
                    None => {
                        let (path, raw_writer) = workspace.create_run()?;
                        empty_meta.file = Some(path);
                        let file = raw_writer
                            .into_inner()
                            .map_err(|err| spill_io_error(workspace, state, err.into_error()))?;
                        empty_writer.insert(BufWriter::with_capacity(writer_buf_bytes, file))
                    }
                };
                let count =
                    write_partition_frame(workspace, state, writer, &comparison, &loc_bytes, rows)?;
                empty_meta.rows = add(empty_meta.rows, 1)?;
                empty_meta.bytes = add(empty_meta.bytes, count)?;
                empty_meta.value_bytes = add(empty_meta.value_bytes, locator.value_bytes)?;
                empty_meta.max_value_bytes = empty_meta.max_value_bytes.max(value_bytes);
                empty_meta
                    .fingerprint
                    .observe_row(rows, locator, &comparison);
                let order = pair_order(
                    config.mode,
                    (side == 0).then_some(rows),
                    (side == 1).then_some(rows),
                );
                let pair_loc = if side == 0 {
                    PairLocator {
                        main_offset: locator.source_offset,
                        main_len: locator.record_len,
                        additional_offset: 0,
                        additional_len: 0,
                    }
                } else {
                    PairLocator {
                        main_offset: 0,
                        main_len: 0,
                        additional_offset: locator.source_offset,
                        additional_len: locator.record_len,
                    }
                };
                empty_meta
                    .pair_fingerprint
                    .observe_pair(order.0, order.1, pair_loc);
                if empty_meta.first_rows.len() < empty_sample_limit {
                    empty_meta.first_rows.push(RowRef {
                        ordinal: rows,
                        source_offset: locator.source_offset,
                        record_len: locator.record_len,
                    });
                }
                count
            } else {
                let h = hash_key(&comparison);
                let part_idx = partition_for_hash(h, partition_count);
                let meta = &mut part_metas[part_idx];
                let writer = match &mut part_writers[part_idx] {
                    Some(w) => w,
                    None => {
                        let (path, raw_writer) = workspace.create_run()?;
                        meta.file = Some(path);
                        let file = raw_writer
                            .into_inner()
                            .map_err(|err| spill_io_error(workspace, state, err.into_error()))?;
                        part_writers[part_idx]
                            .insert(BufWriter::with_capacity(writer_buf_bytes, file))
                    }
                };
                let count =
                    write_partition_frame(workspace, state, writer, &comparison, &loc_bytes, rows)?;
                meta.rows = add(meta.rows, 1)?;
                meta.bytes = add(meta.bytes, count)?;
                meta.key_bytes = add(meta.key_bytes, comparison.len() as u64)?;
                meta.max_key_bytes = meta.max_key_bytes.max(comparison.len());
                meta.fingerprint.observe_row(rows, locator, &comparison);
                count
            };
            total_bytes = add(total_bytes, written)?;
            state.add_disk(written)?;
            rows = add(rows, 1)?;
            Ok(())
        },
    )?;

    if let Some(mut writer) = empty_writer {
        flush_partition_writer(workspace, state, &mut writer)?;
    }
    for mut writer in part_writers.into_iter().flatten() {
        flush_partition_writer(workspace, state, &mut writer)?;
    }
    drop(_fd_guard);
    if rows != input.info.data_rows {
        return Err(QuarryError::SourceChanged.into());
    }
    if total_bytes != input.frame_bytes {
        return Err(error("Prepared join data differs from the inspected input.").into());
    }
    Ok((
        PartitionedInput {
            partitions: part_metas,
            empty: empty_meta,
        },
        overall_fp,
    ))
}

/// Partition both inputs into `P` key-only frame partitions and run parallel hash analysis.
pub(super) fn prepare_and_analyze_hash(
    inspected: [Inspected; 2],
    projections: [Vec<(usize, bool)>; 2],
    output: &OutputLayout,
    workspace: &Arc<SpillWorkspace>,
    config: JoinConfig,
    state: &Arc<Progress>,
) -> JResult<([PreparedInput; 2], HashJoinPlan, JoinAnalysis)> {
    let total_frame_bytes = add(inspected[0].frame_bytes, inspected[1].frame_bytes)?;
    let partition_count = choose_partition_count(total_frame_bytes, config);
    let fd_cap = engine_pool().fd_budget().limit();
    let parallel_partition = fd_cap >= 2 * (partition_count + 2);
    let writer_buf_bytes =
        (config.run_memory_bytes / (4 * partition_count)).clamp(4 * 1024, 64 * 1024);

    state.phase(JoinPhase::Sorting);

    let [left_in, right_in] = inspected;
    let [left_proj, right_proj] = projections;
    let (left_part, left_fp, right_part, right_fp) = if parallel_partition {
        type PartitionInputSlot =
            Arc<Mutex<Option<JResult<(PartitionedInput, LocatorFingerprint)>>>>;
        let left_arc = Arc::new((left_in, left_proj));
        let right_arc = Arc::new((right_in, right_proj));
        let left_res: PartitionInputSlot = Arc::new(Mutex::new(None));
        let right_res: PartitionInputSlot = Arc::new(Mutex::new(None));

        let l_in = Arc::clone(&left_arc);
        let r_in = Arc::clone(&right_arc);
        let l_out = Arc::clone(&left_res);
        let r_out = Arc::clone(&right_res);
        let ws = Arc::clone(workspace);
        let st = Arc::clone(state);

        run_pair_on_engine_pool(move |idx| {
            if idx == 0 {
                let res = partition_one_input(
                    &l_in.0,
                    0,
                    &l_in.1,
                    partition_count,
                    writer_buf_bytes,
                    &ws,
                    config,
                    &st,
                );
                *l_out.lock().unwrap() = Some(res);
            } else {
                let res = partition_one_input(
                    &r_in.0,
                    1,
                    &r_in.1,
                    partition_count,
                    writer_buf_bytes,
                    &ws,
                    config,
                    &st,
                );
                *r_out.lock().unwrap() = Some(res);
            }
        })?;

        let (lp, lfp) = left_res.lock().unwrap().take().expect("left partitioned")?;
        let (rp, rfp) = right_res
            .lock()
            .unwrap()
            .take()
            .expect("right partitioned")?;
        let (li, lproj) = Arc::try_unwrap(left_arc).ok().expect("left workers done");
        let (ri, rproj) = Arc::try_unwrap(right_arc).ok().expect("right workers done");
        return finish_prepare_and_analyze(
            [li, ri],
            [lproj, rproj],
            [lp, rp],
            [lfp, rfp],
            partition_count,
            output,
            workspace,
            config,
            state,
        );
    } else {
        let (lp, lfp) = partition_one_input(
            &left_in,
            0,
            &left_proj,
            partition_count,
            writer_buf_bytes,
            workspace,
            config,
            state,
        )?;
        let (rp, rfp) = partition_one_input(
            &right_in,
            1,
            &right_proj,
            partition_count,
            writer_buf_bytes,
            workspace,
            config,
            state,
        )?;
        (lp, lfp, rp, rfp)
    };

    finish_prepare_and_analyze(
        [left_in, right_in],
        [left_proj, right_proj],
        [left_part, right_part],
        [left_fp, right_fp],
        partition_count,
        output,
        workspace,
        config,
        state,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_prepare_and_analyze(
    inspected: [Inspected; 2],
    projections: [Vec<(usize, bool)>; 2],
    sides: [PartitionedInput; 2],
    fingerprints: [LocatorFingerprint; 2],
    partition_count: usize,
    output: &OutputLayout,
    workspace: &Arc<SpillWorkspace>,
    config: JoinConfig,
    state: &Arc<Progress>,
) -> JResult<([PreparedInput; 2], HashJoinPlan, JoinAnalysis)> {
    let [li, ri] = inspected;
    let [lproj, rproj] = projections;
    let [lfp, rfp] = fingerprints;
    let prepared = [
        PreparedInput {
            info: li.info,
            physical_path: li.physical_path,
            working: li.working,
            stamp: li.stamp,
            storage: li.storage,
            locator_fingerprint: lfp,
            key_columns: li.key_columns,
            require_unique: li.require_unique,
            output_delimiter: li.output_delimiter,
            projection: lproj,
            sorted: None,
            frame_bytes: li.frame_bytes,
        },
        PreparedInput {
            info: ri.info,
            physical_path: ri.physical_path,
            working: ri.working,
            stamp: ri.stamp,
            storage: ri.storage,
            locator_fingerprint: rfp,
            key_columns: ri.key_columns,
            require_unique: ri.require_unique,
            output_delimiter: ri.output_delimiter,
            projection: rproj,
            sorted: None,
            frame_bytes: ri.frame_bytes,
        },
    ];

    let (plan, analysis) = analyze_partitioned(
        &prepared,
        sides,
        partition_count,
        output,
        workspace,
        config,
        state,
    )?;
    Ok((prepared, plan, analysis))
}

const NO_ORDINAL: u64 = u64::MAX;

struct AnalysisGroup {
    hash: u64,
    key_start: u32,
    key_len: u32,
    rows: [u32; 2],
    value_bytes: [u64; 2],
    max_value_bytes: [u32; 2],
    first_ordinal: [u64; 2],
}

enum TableInsertError {
    BudgetExceeded,
    DuplicateUnique { side: usize, record: u64 },
}

/// Reusable arena-backed open-addressed hash table for partition analysis.
struct AnalysisTable {
    key_arena: Vec<u8>,
    groups: Vec<AnalysisGroup>,
    slots: Vec<u32>,
}

impl AnalysisTable {
    fn new() -> Self {
        Self {
            key_arena: Vec::new(),
            groups: Vec::new(),
            slots: Vec::new(),
        }
    }

    fn reset(&mut self, max_retain_bytes: usize) {
        self.key_arena.clear();
        self.groups.clear();
        self.slots.fill(EMPTY_SLOT);
        if self.mem_bytes() > max_retain_bytes {
            self.key_arena = Vec::new();
            self.groups = Vec::new();
            self.slots = Vec::new();
        }
    }

    fn mem_bytes(&self) -> usize {
        self.key_arena
            .capacity()
            .saturating_add(
                self.groups
                    .capacity()
                    .saturating_mul(std::mem::size_of::<AnalysisGroup>()),
            )
            .saturating_add(
                self.slots
                    .capacity()
                    .saturating_mul(std::mem::size_of::<u32>()),
            )
    }

    /// Pre-size exact capacities when the worst-case distinct count fits inside `mem_cap`,
    /// avoiding 2x `Vec` doubling waste and macOS deferred-reclaim churn.
    fn prepare_for_metas(&mut self, metas: [&PartitionMeta; 2], mem_cap: usize) {
        self.reset(mem_cap);
        let Ok(est_groups) = usize::try_from(metas[0].rows.saturating_add(metas[1].rows)) else {
            self.reset(0);
            return;
        };
        let Ok(est_keys) = usize::try_from(metas[0].key_bytes.saturating_add(metas[1].key_bytes))
        else {
            self.reset(0);
            return;
        };
        if est_groups == 0 {
            return;
        }
        let slot_len = (est_groups.saturating_mul(10) / 7 + 1)
            .next_power_of_two()
            .max(64);
        let projected = est_keys
            .saturating_add(est_groups.saturating_mul(std::mem::size_of::<AnalysisGroup>()))
            .saturating_add(slot_len.saturating_mul(std::mem::size_of::<u32>()));
        if projected > mem_cap {
            // High-duplicate partitions may still fit dynamically; drop any incompatible retained
            // buffers from prior partitions so on-demand growth gets the full `mem_cap`.
            self.reset(0);
            return;
        }
        // Account for retained buffers from prior partitions BEFORE reserving any growth so
        // combined capacity never exceeds `mem_cap` even transiently.
        let combined = self
            .key_arena
            .capacity()
            .max(est_keys)
            .saturating_add(
                self.groups
                    .capacity()
                    .max(est_groups)
                    .saturating_mul(std::mem::size_of::<AnalysisGroup>()),
            )
            .saturating_add(
                self.slots
                    .capacity()
                    .max(slot_len)
                    .saturating_mul(std::mem::size_of::<u32>()),
            );
        if combined > mem_cap {
            self.reset(0);
        }
        if self.key_arena.capacity() < est_keys
            && self.key_arena.try_reserve_exact(est_keys).is_err()
        {
            self.reset(0);
            return;
        }
        if self.groups.capacity() < est_groups && self.groups.try_reserve_exact(est_groups).is_err()
        {
            self.reset(0);
            return;
        }
        if self.slots.len() < slot_len {
            self.slots = Vec::new();
            let mut new_slots = Vec::new();
            if new_slots.try_reserve_exact(slot_len).is_err() {
                self.reset(0);
                return;
            }
            new_slots.resize(slot_len, EMPTY_SLOT);
            self.slots = new_slots;
        }
        if self.mem_bytes() > mem_cap {
            self.reset(0);
        }
    }

    fn ensure_capacity_for_insert(
        &mut self,
        extra_key_bytes: usize,
        mem_cap: usize,
    ) -> Result<(), TableInsertError> {
        let need_slots = self.slots.is_empty()
            || (self.groups.len() + 1).saturating_mul(10) > self.slots.len().saturating_mul(7);
        let next_slots_cap = if need_slots {
            if self.slots.is_empty() {
                64
            } else {
                self.slots.len().saturating_mul(2)
            }
        } else {
            self.slots.capacity()
        };
        let need_group_grow = self.groups.len() == self.groups.capacity();
        let next_groups_cap = if need_group_grow {
            self.groups.capacity().saturating_mul(2).max(32)
        } else {
            self.groups.capacity()
        };
        let needed_arena = self.key_arena.len().saturating_add(extra_key_bytes);
        let need_arena_grow = needed_arena > self.key_arena.capacity();
        let next_arena_cap = if need_arena_grow {
            needed_arena
                .max(self.key_arena.capacity().saturating_mul(2))
                .max(1024)
        } else {
            self.key_arena.capacity()
        };
        let projected = next_arena_cap
            .saturating_add(next_groups_cap.saturating_mul(std::mem::size_of::<AnalysisGroup>()))
            .saturating_add(next_slots_cap.saturating_mul(std::mem::size_of::<u32>()));
        if projected > mem_cap {
            // Try bounded exact reservation instead of 2x doubling before declaring budget exceeded.
            let exact_groups_cap = if need_group_grow {
                self.groups.len().saturating_add(1)
            } else {
                self.groups.capacity()
            };
            let exact_arena_cap = needed_arena.max(self.key_arena.capacity());
            let exact = exact_arena_cap
                .saturating_add(
                    exact_groups_cap.saturating_mul(std::mem::size_of::<AnalysisGroup>()),
                )
                .saturating_add(next_slots_cap.saturating_mul(std::mem::size_of::<u32>()));
            if exact > mem_cap {
                return Err(TableInsertError::BudgetExceeded);
            }
            if need_group_grow {
                self.groups
                    .try_reserve_exact(1)
                    .map_err(|_| TableInsertError::BudgetExceeded)?;
            }
            if need_arena_grow {
                self.key_arena
                    .try_reserve_exact(extra_key_bytes)
                    .map_err(|_| TableInsertError::BudgetExceeded)?;
            }
        }
        if need_slots {
            let new_len = next_slots_cap;
            let mut new_slots = Vec::new();
            new_slots
                .try_reserve_exact(new_len)
                .map_err(|_| TableInsertError::BudgetExceeded)?;
            new_slots.resize(new_len, EMPTY_SLOT);
            let mask = new_len - 1;
            for (idx, group) in self.groups.iter().enumerate() {
                let mut pos = (group.hash as usize) & mask;
                while new_slots[pos] != EMPTY_SLOT {
                    pos = (pos + 1) & mask;
                }
                new_slots[pos] = (idx + 1) as u32;
            }
            self.slots = new_slots;
        }
        if self.mem_bytes() > mem_cap {
            return Err(TableInsertError::BudgetExceeded);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_row(
        &mut self,
        side: usize,
        key: &[u8],
        ordinal: u64,
        locator: RowLocator,
        has_header: bool,
        require_unique: bool,
        mem_cap: usize,
    ) -> Result<(), TableInsertError> {
        let Ok(value_bytes_u32) = u32::try_from(locator.value_bytes) else {
            return Err(TableInsertError::BudgetExceeded);
        };
        let h = hash_key(key);
        if !self.slots.is_empty() {
            let mask = self.slots.len() - 1;
            let mut pos = (h as usize) & mask;
            loop {
                let slot = self.slots[pos];
                if slot == EMPTY_SLOT {
                    break;
                }
                let group = &mut self.groups[(slot - 1) as usize];
                if group.hash == h
                    && group.key_len as usize == key.len()
                    && &self.key_arena
                        [group.key_start as usize..group.key_start as usize + key.len()]
                        == key
                {
                    let Some(next_rows) = group.rows[side].checked_add(1) else {
                        return Err(TableInsertError::BudgetExceeded);
                    };
                    group.rows[side] = next_rows;
                    if group.rows[side] > 1 && require_unique {
                        let record = ordinal.saturating_add(1 + u64::from(has_header));
                        return Err(TableInsertError::DuplicateUnique { side, record });
                    }
                    group.value_bytes[side] =
                        group.value_bytes[side].saturating_add(locator.value_bytes);
                    group.max_value_bytes[side] = group.max_value_bytes[side].max(value_bytes_u32);
                    if group.first_ordinal[side] == NO_ORDINAL {
                        group.first_ordinal[side] = ordinal;
                    }
                    return Ok(());
                }
                pos = (pos + 1) & mask;
            }
        }

        if u32::try_from(self.key_arena.len()).is_err()
            || u32::try_from(key.len()).is_err()
            || u32::try_from(self.groups.len() + 1).is_err()
        {
            return Err(TableInsertError::BudgetExceeded);
        }
        self.ensure_capacity_for_insert(key.len(), mem_cap)?;
        let key_start = self.key_arena.len() as u32;
        self.key_arena.extend_from_slice(key);
        let group_idx = self.groups.len();
        let mut group = AnalysisGroup {
            hash: h,
            key_start,
            key_len: key.len() as u32,
            rows: [0, 0],
            value_bytes: [0, 0],
            max_value_bytes: [0, 0],
            first_ordinal: [NO_ORDINAL, NO_ORDINAL],
        };
        group.rows[side] = 1;
        group.value_bytes[side] = locator.value_bytes;
        group.max_value_bytes[side] = value_bytes_u32;
        group.first_ordinal[side] = ordinal;
        self.groups.push(group);

        let mask = self.slots.len() - 1;
        let mut pos = (h as usize) & mask;
        while self.slots[pos] != EMPTY_SLOT {
            pos = (pos + 1) & mask;
        }
        self.slots[pos] = (group_idx + 1) as u32;
        Ok(())
    }

    fn key_bytes(&self, group: &AnalysisGroup) -> &[u8] {
        let start = group.key_start as usize;
        &self.key_arena[start..start + group.key_len as usize]
    }

    #[inline]
    fn find(&self, key: &[u8]) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let h = hash_key(key);
        let mask = self.slots.len() - 1;
        let mut pos = (h as usize) & mask;
        loop {
            let slot = self.slots[pos];
            if slot == EMPTY_SLOT {
                return None;
            }
            let idx = (slot - 1) as usize;
            let group = &self.groups[idx];
            if group.hash == h
                && group.key_len as usize == key.len()
                && &self.key_arena[group.key_start as usize..group.key_start as usize + key.len()]
                    == key
            {
                return Some(idx);
            }
            pos = (pos + 1) & mask;
        }
    }
}

#[derive(Clone, Copy)]
struct PreviewCandidate {
    main_row: Option<RowRef>,
    additional_row: Option<RowRef>,
    left_key: Option<KeyDigest>,
    right_key: Option<KeyDigest>,
}

struct PartitionAnalysisOut {
    plan: PartitionPlan,
    matching: [u64; 2],
    pairs: u64,
    unmatched: [u64; 2],
    duplicate_groups: [u64; 2],
    many_to_many_groups: u64,
    many_to_many_pairs: u64,
    output_rows: u64,
    output_bytes: u64,
    duplicate_samples: BTreeMap<SampleKey, JoinDuplicateExample>,
    unmatched_samples: [BTreeMap<SampleKey, JoinUnmatchedExample>; 2],
    preview_candidates: BTreeMap<(u64, u64), PreviewCandidate>,
    unique_error: Option<(usize, u64)>,
}

impl PartitionAnalysisOut {
    fn empty(plan: PartitionPlan) -> Self {
        Self {
            plan,
            matching: [0, 0],
            pairs: 0,
            unmatched: [0, 0],
            duplicate_groups: [0, 0],
            many_to_many_groups: 0,
            many_to_many_pairs: 0,
            output_rows: 0,
            output_bytes: 0,
            duplicate_samples: BTreeMap::new(),
            unmatched_samples: [BTreeMap::new(), BTreeMap::new()],
            preview_candidates: BTreeMap::new(),
            unique_error: None,
        }
    }
}

fn estimate_analysis_ram(left: &PartitionMeta, right: &PartitionMeta) -> usize {
    let est_groups = left.rows.saturating_add(right.rows).min(usize::MAX as u64) as usize;
    let est_keys = left
        .key_bytes
        .saturating_add(right.key_bytes)
        .min(usize::MAX as u64) as usize;
    let slots = (est_groups.saturating_mul(10) / 7 + 1)
        .next_power_of_two()
        .max(64);
    est_keys
        .saturating_add(est_groups.saturating_mul(std::mem::size_of::<AnalysisGroup>()))
        .saturating_add(slots.saturating_mul(std::mem::size_of::<u32>()))
}

fn estimate_build_ram(meta: &PartitionMeta) -> usize {
    let rows = meta.rows.min(usize::MAX as u64) as usize;
    let keys = meta.key_bytes.min(usize::MAX as u64) as usize;
    let slots = (rows.saturating_mul(10) / 7 + 1)
        .next_power_of_two()
        .max(64);
    keys.saturating_add(rows.saturating_mul(std::mem::size_of::<BuildRow>()))
        .saturating_add(rows.saturating_mul(std::mem::size_of::<BuildBucket>()))
        .saturating_add(slots.saturating_mul(std::mem::size_of::<u32>()))
}

fn insert_bounded_sample<V>(
    map: &mut BTreeMap<SampleKey, V>,
    key: SampleKey,
    value: V,
    limit: usize,
) {
    if limit == 0 {
        return;
    }
    if map.len() >= limit && map.last_key_value().is_some_and(|(last, _)| &key >= last) {
        return;
    }
    map.insert(key, value);
    while map.len() > limit {
        map.pop_last();
    }
}

fn insert_bounded_preview(
    map: &mut BTreeMap<(u64, u64), PreviewCandidate>,
    order: (u64, u64),
    candidate: PreviewCandidate,
    limit: usize,
) {
    if limit == 0 {
        return;
    }
    if map.len() >= limit && map.last_key_value().is_some_and(|(&last, _)| order >= last) {
        return;
    }
    map.insert(order, candidate);
    while map.len() > limit {
        map.pop_last();
    }
}

fn cleanup_fallback_runs<T>(
    primary: JResult<T>,
    runs: &[(&SpillFile, u64)],
    state: &Progress,
) -> JResult<T> {
    let mut cleanup_err: Option<QuarryError> = None;
    for &(file, bytes) in runs {
        match file.remove() {
            Ok(()) => state.sub_disk(bytes),
            Err(err) => {
                if cleanup_err.is_none() {
                    cleanup_err = Some(err.into());
                }
            }
        }
    }
    match (primary, cleanup_err) {
        (Ok(val), None) => Ok(val),
        (Ok(_), Some(err)) => Err(err.into()),
        (Err(primary_err), None) => Err(primary_err),
        (Err(primary_err), Some(err)) => Err(error(format!(
            "{}. Could not remove private fallback sort data: {err}",
            primary_err.description()
        ))
        .into()),
    }
}

/// Sort one partition file into a temporary sorted run using ADR 0011 `Runs`, validating its
/// `LocatorFingerprint` and strict ordinal monotonicity.
fn sort_partition_file(
    input: &PreparedInput,
    meta: &PartitionMeta,
    workspace: &SpillWorkspace,
    config: JoinConfig,
    state: &Progress,
) -> JResult<Run> {
    let mut runs = Runs::new(workspace, config, state, meta.max_key_bytes.max(16));
    let Some(file) = meta.file.as_ref() else {
        return runs.finish();
    };
    let mut reader = BufReader::with_capacity(READ_CHUNK, file.open()?);
    let mut seen_rows = 0_u64;
    let mut last_ordinal: Option<u64> = None;
    let mut observed_fp = LocatorFingerprint::default();
    while let Some(head) = read_head(&mut reader, config, state)? {
        if last_ordinal.is_some_and(|last| head.ordinal <= last) {
            return Err(error("Private join ordinals are out of order.").into());
        }
        let raw = read_exact_fixed::<ROW_LOCATOR_BYTES>(&mut reader, head.record_len, state)?;
        let locator = RowLocator::from_le_bytes(&raw);
        validate_row_locator(locator, input, head.ordinal, config)?;
        observed_fp.observe_row(head.ordinal, locator, &head.key);
        runs.push(&head.key, EntryPayload::Locator(raw), head.ordinal)?;
        last_ordinal = Some(head.ordinal);
        seen_rows = add(seen_rows, 1)?;
    }
    if seen_rows != meta.rows || observed_fp != meta.fingerprint {
        return Err(error("Private join row locators changed.").into());
    }
    let sorted = runs.finish()?;
    if sorted.bytes != meta.bytes {
        return Err(error("Prepared join partition size changed.").into());
    }
    Ok(sorted)
}

fn temporary_prepared_view(
    input: &PreparedInput,
    meta: &PartitionMeta,
    sorted: SpillFile,
) -> PreparedInput {
    PreparedInput {
        info: JoinInputInfo {
            path: input.info.path.clone(),
            working_values: input.info.working_values,
            delimiter: input.info.delimiter,
            encoding: input.info.encoding,
            columns: Vec::new(),
            has_header: input.info.has_header,
            data_rows: input.info.data_rows,
            empty_key_rows: 0,
            bytes: input.info.bytes,
        },
        physical_path: input.physical_path.clone(),
        working: None,
        stamp: input.stamp.clone(),
        storage: FrameStorage::Seekable,
        locator_fingerprint: meta.fingerprint,
        key_columns: input.key_columns.clone(),
        require_unique: input.require_unique,
        output_delimiter: input.output_delimiter,
        projection: Vec::new(),
        sorted: Some(sorted),
        frame_bytes: meta.bytes,
    }
}

/// Analyze one partition via ADR 0011 external sort-merge fallback.
#[allow(clippy::too_many_arguments)]
fn analyze_partition_sort_merge(
    inputs: &[PreparedInput; 2],
    metas: [&PartitionMeta; 2],
    columns: u64,
    workspace: &SpillWorkspace,
    config: JoinConfig,
    state: &Progress,
) -> JResult<PartitionAnalysisOut> {
    #[cfg(test)]
    storage_test::record_hash_fallback();

    let sorted0 = sort_partition_file(&inputs[0], metas[0], workspace, config, state)?;
    let sorted1 = match sort_partition_file(&inputs[1], metas[1], workspace, config, state) {
        Ok(run) => run,
        Err(err) => {
            return cleanup_fallback_runs(Err(err), &[(&sorted0.path, sorted0.bytes)], state);
        }
    };
    let s0_path = sorted0.path.clone();
    let s1_path = sorted1.path.clone();
    let s0_bytes = sorted0.bytes;
    let s1_bytes = sorted1.bytes;

    let result = (|| -> JResult<PartitionAnalysisOut> {
        let mut view0 = temporary_prepared_view(&inputs[0], metas[0], sorted0.path);
        let mut view1 = temporary_prepared_view(&inputs[1], metas[1], sorted1.path);
        // Temporary views expect `meta.rows` rather than the full input's `data_rows` at EOF in `Groups::next`.
        view0.info.data_rows = metas[0].rows;
        view1.info.data_rows = metas[1].rows;
        let views = [view0, view1];

        let build_side = usize::from(estimate_build_ram(metas[1]) < estimate_build_ram(metas[0]));
        let fallback_extra = mul(add(metas[0].bytes, metas[1].bytes)?, 2)?;
        let mut out = PartitionAnalysisOut::empty(PartitionPlan {
            build_side,
            use_sort_merge: true,
            max_group_pairs: 0,
            fallback_extra_bytes: fallback_extra,
        });
        let mut range_readers: [Option<BufReader<File>>; 2] = [None, None];
        // Note: `Groups::next` validates `ordinal < info.data_rows`, so validate against global bounds
        // by passing the actual `inputs[side].info.data_rows` in `validate_row_locator` via a dedicated
        // walk using `sorted_views_with_expected_rows`.
        let walk_result =
            walk_partition_sorted(inputs, &views, metas, config, state, |left, right| {
                for (side, group) in [left.as_ref(), right.as_ref()].into_iter().enumerate() {
                    if group.is_some_and(|g| g.key[0] == 1 && g.range.rows > 1) {
                        out.duplicate_groups[side] = add(out.duplicate_groups[side], 1)?;
                    }
                }
                if left.as_ref().is_some_and(|g| g.range.rows > 1)
                    || right.as_ref().is_some_and(|g| g.range.rows > 1)
                {
                    let key = &left.as_ref().or(right.as_ref()).unwrap().key;
                    insert_bounded_sample(
                        &mut out.duplicate_samples,
                        SampleKey::from_bytes(key),
                        JoinDuplicateExample {
                            key: key_label(key)?,
                            main_rows: left.as_ref().map_or(0, |g| g.range.rows),
                            additional_rows: right.as_ref().map_or(0, |g| g.range.rows),
                        },
                        MAX_GROUP_EXAMPLES,
                    );
                }
                match (&left, &right) {
                    (Some(group), None) => {
                        insert_bounded_sample(
                            &mut out.unmatched_samples[0],
                            SampleKey::from_bytes(&group.key),
                            JoinUnmatchedExample {
                                data_row: group.first_ordinal + 1,
                                key: Some(key_label(&group.key)?),
                            },
                            MAX_GROUP_EXAMPLES,
                        );
                    }
                    (None, Some(group)) => {
                        insert_bounded_sample(
                            &mut out.unmatched_samples[1],
                            SampleKey::from_bytes(&group.key),
                            JoinUnmatchedExample {
                                data_row: group.first_ordinal + 1,
                                key: Some(key_label(&group.key)?),
                            },
                            MAX_GROUP_EXAMPLES,
                        );
                    }
                    _ => {}
                }
                let min_order = pair_order(
                    config.mode,
                    left.as_ref().map(|g| g.first_ordinal),
                    right.as_ref().map(|g| g.first_ordinal),
                );
                let may_improve_preview = config.preview_rows > 0
                    && config.mode.retains(left.is_some(), right.is_some())
                    && (out.preview_candidates.len() < config.preview_rows
                        || out
                            .preview_candidates
                            .last_key_value()
                            .is_some_and(|(&last, _)| min_order < last));
                let left_key = may_improve_preview
                    .then(|| left.as_ref().map(|g| KeyDigest::from_bytes(&g.key)))
                    .flatten();
                let right_key = may_improve_preview
                    .then(|| right.as_ref().map(|g| KeyDigest::from_bytes(&g.key)))
                    .flatten();
                let left_data = left.map(Group::into_data);
                let right_data = right.map(Group::into_data);
                match (
                    left_data.as_ref().map(|g| g.range),
                    right_data.as_ref().map(|g| g.range),
                ) {
                    (Some(a), Some(b)) => {
                        let (count, bytes) =
                            matched_group_output(a, b, columns, config.max_record_bytes)?;
                        out.matching[0] = add(out.matching[0], a.rows)?;
                        out.matching[1] = add(out.matching[1], b.rows)?;
                        out.pairs = add(out.pairs, count)?;
                        out.plan.max_group_pairs = out.plan.max_group_pairs.max(count);
                        if a.rows > 1 && b.rows > 1 {
                            out.many_to_many_groups = add(out.many_to_many_groups, 1)?;
                            out.many_to_many_pairs = add(out.many_to_many_pairs, count)?;
                        }
                        out.output_rows = add(out.output_rows, count)?;
                        out.output_bytes = add(out.output_bytes, bytes)?;
                    }
                    (a, b) => {
                        let (side, group) = if let Some(a) = a {
                            (0, a)
                        } else {
                            (1, b.unwrap())
                        };
                        out.unmatched[side] = add(out.unmatched[side], group.rows)?;
                        if config.mode.retains(a.is_some(), b.is_some()) {
                            out.output_rows = add(out.output_rows, group.rows)?;
                            out.output_bytes = add(
                                out.output_bytes,
                                add(group.value_bytes, mul(group.rows, columns)?)?,
                            )?;
                            if group.max_value_bytes.saturating_add(columns as usize)
                                > config.max_record_bytes
                            {
                                return Err(error(
                                    "A joined record exceeds the configured size limit.",
                                )
                                .into());
                            }
                        }
                    }
                }
                if may_improve_preview {
                    // Use full global row counts during RangeRows validation.
                    let full_views = [
                        PreparedInput {
                            info: JoinInputInfo {
                                data_rows: inputs[0].info.data_rows,
                                ..temporary_info_clone(&views[0].info)
                            },
                            ..temporary_shallow_view(&views[0])
                        },
                        PreparedInput {
                            info: JoinInputInfo {
                                data_rows: inputs[1].info.data_rows,
                                ..temporary_info_clone(&views[1].info)
                            },
                            ..temporary_shallow_view(&views[1])
                        },
                    ];
                    enumerate_pairs(
                        &full_views,
                        left_data,
                        right_data,
                        &mut range_readers,
                        config,
                        state,
                        Some(config.preview_rows),
                        |a, b| {
                            let main_row = a.map(|item| match item {
                                GroupItem::Ref(r) => *r,
                                GroupItem::Owned(_) => unreachable!(),
                            });
                            let add_row = b.map(|item| match item {
                                GroupItem::Ref(r) => *r,
                                GroupItem::Owned(_) => unreachable!(),
                            });
                            let order = pair_order(
                                config.mode,
                                main_row.map(|r| r.ordinal),
                                add_row.map(|r| r.ordinal),
                            );
                            insert_bounded_preview(
                                &mut out.preview_candidates,
                                order,
                                PreviewCandidate {
                                    main_row,
                                    additional_row: add_row,
                                    left_key,
                                    right_key,
                                },
                                config.preview_rows,
                            );
                            Ok(())
                        },
                    )?;
                }
                Ok(())
            });
        drop(range_readers);
        if let Err(err) = walk_result {
            if let Failure::Error(QuarryError::JoinFile {
                path: Some(ref p),
                record: Some(rec),
                ref reason,
            }) = err
                && reason.contains("Require unique key")
            {
                let side = usize::from(*p == inputs[1].info.path);
                out.unique_error = Some((side, rec));
                return Ok(out);
            }
            return Err(err);
        }
        Ok(out)
    })();

    cleanup_fallback_runs(result, &[(&s0_path, s0_bytes), (&s1_path, s1_bytes)], state)
}

fn temporary_info_clone(info: &JoinInputInfo) -> JoinInputInfo {
    JoinInputInfo {
        path: info.path.clone(),
        working_values: info.working_values,
        delimiter: info.delimiter,
        encoding: info.encoding,
        columns: Vec::new(),
        has_header: info.has_header,
        data_rows: info.data_rows,
        empty_key_rows: info.empty_key_rows,
        bytes: info.bytes,
    }
}

fn temporary_shallow_view(view: &PreparedInput) -> PreparedInput {
    PreparedInput {
        info: temporary_info_clone(&view.info),
        physical_path: view.physical_path.clone(),
        working: None,
        stamp: view.stamp.clone(),
        storage: view.storage,
        locator_fingerprint: view.locator_fingerprint,
        key_columns: view.key_columns.clone(),
        require_unique: view.require_unique,
        output_delimiter: view.output_delimiter,
        projection: Vec::new(),
        sorted: view.sorted.clone(),
        frame_bytes: view.frame_bytes,
    }
}

/// Walk two sorted partition streams with `Groups`, validating global ordinal bounds against
/// `full_inputs` and exact partition row count / `LocatorFingerprint` against `metas`.
fn walk_partition_sorted(
    full_inputs: &[PreparedInput; 2],
    views: &[PreparedInput; 2],
    metas: [&PartitionMeta; 2],
    config: JoinConfig,
    state: &Progress,
    mut visit: impl FnMut(Option<Group>, Option<Group>) -> JResult<()>,
) -> JResult<()> {
    let mut left = PartitionGroups::new(
        &full_inputs[0],
        views[0].sorted.as_ref().unwrap(),
        metas[0],
        config,
        state,
    )?;
    let mut right = PartitionGroups::new(
        &full_inputs[1],
        views[1].sorted.as_ref().unwrap(),
        metas[1],
        config,
        state,
    )?;
    let mut a = left.next()?;
    let mut b = right.next()?;
    loop {
        state.check()?;
        let order = match (&a, &b) {
            (None, None) => break,
            (Some(_), None) => CmpOrdering::Less,
            (None, Some(_)) => CmpOrdering::Greater,
            (Some(a), Some(_)) if a.key[0] == 0 => CmpOrdering::Less,
            (Some(_), Some(b)) if b.key[0] == 0 => CmpOrdering::Greater,
            (Some(a), Some(b)) => a.key.cmp(&b.key),
        };
        match order {
            CmpOrdering::Less => {
                visit(a.take(), None)?;
                a = left.next()?;
            }
            CmpOrdering::Greater => {
                visit(None, b.take())?;
                b = right.next()?;
            }
            CmpOrdering::Equal => {
                visit(a.take(), b.take())?;
                a = left.next()?;
                b = right.next()?;
            }
        }
    }
    Ok(())
}

struct PartitionGroups<'a> {
    input: &'a PreparedInput,
    meta: &'a PartitionMeta,
    reader: BufReader<File>,
    config: JoinConfig,
    state: &'a Progress,
    rows: u64,
    position: u64,
    peeked: Option<FrameHead>,
    observed_fp: LocatorFingerprint,
}

impl<'a> PartitionGroups<'a> {
    fn new(
        input: &'a PreparedInput,
        sorted: &SpillFile,
        meta: &'a PartitionMeta,
        config: JoinConfig,
        state: &'a Progress,
    ) -> JResult<Self> {
        Ok(Self {
            input,
            meta,
            reader: BufReader::with_capacity(READ_CHUNK, sorted.open()?),
            config,
            state,
            rows: 0,
            position: 0,
            peeked: None,
            observed_fp: LocatorFingerprint::default(),
        })
    }

    fn next(&mut self) -> JResult<Option<Group>> {
        self.state.check()?;
        let start = self.position;
        let Some(first) = (match self.peeked.take() {
            Some(head) => Some(head),
            None => read_head(&mut self.reader, self.config, self.state)?,
        }) else {
            if self.rows != self.meta.rows || self.observed_fp != self.meta.fingerprint {
                return Err(error("Private join row locators changed.").into());
            }
            return Ok(None);
        };
        if first.key.is_empty() {
            return Err(error("Empty private join key.").into());
        }
        let FrameHead {
            key,
            record_len,
            ordinal,
        } = first;
        self.position = add(self.position, add(24, key.len() as u64)?)?;
        let mut head = (record_len, ordinal);
        let mut rows = 0_u64;
        let mut value_bytes = 0_u64;
        let mut max_value_bytes = 0_usize;
        let mut last_ordinal = None;
        let inline_cap = self.config.group_memory_bytes.min(64 * 1024);
        let mut inline: Option<Vec<GroupItem>> = Some(Vec::new());
        let mut inline_bytes = 0_usize;
        let end;
        loop {
            self.state.check()?;
            if last_ordinal.is_some_and(|last| head.1 <= last) {
                return Err(error("Private join ordinals are out of order.").into());
            }
            let bytes =
                read_exact_fixed::<ROW_LOCATOR_BYTES>(&mut self.reader, head.0, self.state)?;
            let locator = RowLocator::from_le_bytes(&bytes);
            validate_row_locator(locator, self.input, head.1, self.config)?;
            self.observed_fp.observe_row(head.1, locator, &key);
            let item = GroupItem::Ref(RowRef {
                ordinal: head.1,
                source_offset: locator.source_offset,
                record_len: locator.record_len,
            });
            let size = locator.value_bytes as usize;
            self.position = add(self.position, head.0 as u64)?;
            last_ordinal = Some(head.1);
            rows = add(rows, 1)?;
            self.rows = add(self.rows, 1)?;
            value_bytes = add(value_bytes, size as u64)?;
            max_value_bytes = max_value_bytes.max(size);
            if let Some(items) = inline.as_mut() {
                if items.try_reserve_exact(1).is_ok() {
                    let next_bytes = inline_bytes.saturating_add(item.mem_bytes());
                    if !items.is_empty() && next_bytes > inline_cap {
                        inline = None;
                    } else {
                        inline_bytes = next_bytes;
                        items.push(item);
                    }
                } else {
                    inline = None;
                }
            }
            if rows > 1 && key[0] == 1 && self.input.require_unique {
                return Err(input_error(
                    &self.input.info.path,
                    Some(add(head.1, 1 + u64::from(self.input.info.has_header))?),
                    "Repeated key found, but Require unique key is enabled for this input.",
                )
                .into());
            }
            let next_position = self.position;
            let Some(next) = read_head(&mut self.reader, self.config, self.state)? else {
                end = next_position;
                break;
            };
            if next.key != key {
                if next.key < key {
                    return Err(error("Private join groups are out of order.").into());
                }
                self.peeked = Some(next);
                end = next_position;
                break;
            }
            self.position = add(self.position, add(24, next.key.len() as u64)?)?;
            head = (next.record_len, next.ordinal);
        }
        Ok(Some(Group {
            key,
            first_ordinal: ordinal,
            range: GroupRange {
                start,
                end,
                rows,
                value_bytes,
                max_value_bytes,
            },
            inline,
        }))
    }
}

/// Analyze one partition using the reusable in-memory `AnalysisTable`; returns `Ok(None)` if the
/// partition exceeded `mem_cap` and must fall back to external sort-merge.
#[allow(clippy::too_many_arguments)]
fn analyze_partition_hash(
    inputs: &[PreparedInput; 2],
    metas: [&PartitionMeta; 2],
    columns: u64,
    table: &mut AnalysisTable,
    mem_cap: usize,
    create_mem_cap: usize,
    config: JoinConfig,
    state: &Progress,
) -> JResult<Option<PartitionAnalysisOut>> {
    table.prepare_for_metas(metas, mem_cap);
    #[cfg(test)]
    {
        storage_test::maybe_pause_first_hash_worker(&state.cancel);
        state.check()?;
    }
    let mut earliest_unique: Option<(usize, u64)> = None;

    for side in 0..2 {
        let Some(file) = metas[side].file.as_ref() else {
            continue;
        };
        let mut reader = BufReader::with_capacity(READ_CHUNK, file.open()?);
        let mut seen_rows = 0_u64;
        let mut last_ordinal: Option<u64> = None;
        let mut observed_fp = LocatorFingerprint::default();
        while let Some(head) = read_head(&mut reader, config, state)? {
            state.check()?;
            if last_ordinal.is_some_and(|last| head.ordinal <= last) {
                return Err(error("Private join ordinals are out of order.").into());
            }
            let raw = read_exact_fixed::<ROW_LOCATOR_BYTES>(&mut reader, head.record_len, state)?;
            let locator = RowLocator::from_le_bytes(&raw);
            validate_row_locator(locator, &inputs[side], head.ordinal, config)?;
            observed_fp.observe_row(head.ordinal, locator, &head.key);
            match table.insert_row(
                side,
                &head.key,
                head.ordinal,
                locator,
                inputs[side].info.has_header,
                inputs[side].require_unique,
                mem_cap,
            ) {
                Ok(()) => {}
                Err(TableInsertError::BudgetExceeded) => {
                    table.reset(0);
                    return Ok(None);
                }
                Err(TableInsertError::DuplicateUnique { side, record }) => {
                    let replace = earliest_unique
                        .is_none_or(|(prev_s, prev_r)| (side, record) < (prev_s, prev_r));
                    if replace {
                        earliest_unique = Some((side, record));
                    }
                    // If Main already failed uniqueness, no need to scan further in this partition.
                    if side == 0 {
                        let mut out = PartitionAnalysisOut::empty(PartitionPlan {
                            build_side: 0,
                            use_sort_merge: false,
                            max_group_pairs: 0,
                            fallback_extra_bytes: 0,
                        });
                        out.unique_error = earliest_unique;
                        return Ok(Some(out));
                    }
                }
            }
            last_ordinal = Some(head.ordinal);
            seen_rows = add(seen_rows, 1)?;
        }
        if seen_rows != metas[side].rows || observed_fp != metas[side].fingerprint {
            return Err(error("Private join row locators changed.").into());
        }
    }

    let est_build_0 = estimate_build_ram(metas[0]);
    let est_build_1 = estimate_build_ram(metas[1]);
    let build_side = usize::from(est_build_1 < est_build_0);
    let mut out = PartitionAnalysisOut::empty(PartitionPlan {
        build_side,
        use_sort_merge: false,
        max_group_pairs: 0,
        fallback_extra_bytes: 0,
    });
    if let Some(unique_err) = earliest_unique {
        out.unique_error = Some(unique_err);
        return Ok(Some(out));
    }

    // Track up to `config.preview_rows` groups that may contribute to the top preview rows.
    let mut preview_groups: BTreeMap<(u64, u64), usize> = BTreeMap::new();

    for (idx, group) in table.groups.iter().enumerate() {
        state.check()?;
        for side in 0..2 {
            if group.rows[side] > 1 {
                out.duplicate_groups[side] = add(out.duplicate_groups[side], 1)?;
            }
        }
        if group.rows[0] > 1 || group.rows[1] > 1 {
            let key = table.key_bytes(group);
            if out.duplicate_samples.len() < MAX_GROUP_EXAMPLES
                || out
                    .duplicate_samples
                    .last_key_value()
                    .is_some_and(|(last, _)| last.cmp_raw(key) == CmpOrdering::Greater)
            {
                insert_bounded_sample(
                    &mut out.duplicate_samples,
                    SampleKey::from_bytes(key),
                    JoinDuplicateExample {
                        key: key_label(key)?,
                        main_rows: u64::from(group.rows[0]),
                        additional_rows: u64::from(group.rows[1]),
                    },
                    MAX_GROUP_EXAMPLES,
                );
            }
        }
        let has_main = group.rows[0] > 0;
        let has_add = group.rows[1] > 0;
        if has_main && !has_add {
            let key = table.key_bytes(group);
            if out.unmatched_samples[0].len() < MAX_GROUP_EXAMPLES
                || out.unmatched_samples[0]
                    .last_key_value()
                    .is_some_and(|(last, _)| last.cmp_raw(key) == CmpOrdering::Greater)
            {
                insert_bounded_sample(
                    &mut out.unmatched_samples[0],
                    SampleKey::from_bytes(key),
                    JoinUnmatchedExample {
                        data_row: group.first_ordinal[0] + 1,
                        key: Some(key_label(key)?),
                    },
                    MAX_GROUP_EXAMPLES,
                );
            }
        } else if !has_main && has_add {
            let key = table.key_bytes(group);
            if out.unmatched_samples[1].len() < MAX_GROUP_EXAMPLES
                || out.unmatched_samples[1]
                    .last_key_value()
                    .is_some_and(|(last, _)| last.cmp_raw(key) == CmpOrdering::Greater)
            {
                insert_bounded_sample(
                    &mut out.unmatched_samples[1],
                    SampleKey::from_bytes(key),
                    JoinUnmatchedExample {
                        data_row: group.first_ordinal[1] + 1,
                        key: Some(key_label(key)?),
                    },
                    MAX_GROUP_EXAMPLES,
                );
            }
        }

        if has_main && has_add {
            let a = GroupRange {
                start: 0,
                end: 0,
                rows: u64::from(group.rows[0]),
                value_bytes: group.value_bytes[0],
                max_value_bytes: group.max_value_bytes[0] as usize,
            };
            let b = GroupRange {
                start: 0,
                end: 0,
                rows: u64::from(group.rows[1]),
                value_bytes: group.value_bytes[1],
                max_value_bytes: group.max_value_bytes[1] as usize,
            };
            let (count, bytes) = matched_group_output(a, b, columns, config.max_record_bytes)?;
            out.matching[0] = add(out.matching[0], a.rows)?;
            out.matching[1] = add(out.matching[1], b.rows)?;
            out.pairs = add(out.pairs, count)?;
            out.plan.max_group_pairs = out.plan.max_group_pairs.max(count);
            if a.rows > 1 && b.rows > 1 {
                out.many_to_many_groups = add(out.many_to_many_groups, 1)?;
                out.many_to_many_pairs = add(out.many_to_many_pairs, count)?;
            }
            out.output_rows = add(out.output_rows, count)?;
            out.output_bytes = add(out.output_bytes, bytes)?;
        } else {
            let side = usize::from(has_add);
            let rows = u64::from(group.rows[side]);
            out.unmatched[side] = add(out.unmatched[side], rows)?;
            if config.mode.retains(has_main, has_add) {
                out.output_rows = add(out.output_rows, rows)?;
                out.output_bytes = add(
                    out.output_bytes,
                    add(group.value_bytes[side], mul(rows, columns)?)?,
                )?;
                if (group.max_value_bytes[side] as usize).saturating_add(columns as usize)
                    > config.max_record_bytes
                {
                    return Err(error("A joined record exceeds the configured size limit.").into());
                }
            }
        }

        if config.preview_rows > 0 && config.mode.retains(has_main, has_add) {
            let main_ord = (group.first_ordinal[0] != NO_ORDINAL).then_some(group.first_ordinal[0]);
            let add_ord = (group.first_ordinal[1] != NO_ORDINAL).then_some(group.first_ordinal[1]);
            let min_order = pair_order(config.mode, main_ord, add_ord);
            if preview_groups.len() < config.preview_rows
                || preview_groups
                    .last_key_value()
                    .is_some_and(|(&last, _)| min_order < last)
            {
                preview_groups.insert(min_order, idx);
                while preview_groups.len() > config.preview_rows {
                    preview_groups.pop_last();
                }
            }
        }
    }

    // Collect exact preview RowRefs for the <= `preview_rows` qualifying groups in this partition,
    // looking up group indices directly in `table` (zero key cloning) while re-validating every
    // partition frame's ordinal order, bounds, and `LocatorFingerprint`.
    if !preview_groups.is_empty() {
        let mut preview_group_slot: HashMap<usize, usize> =
            HashMap::with_capacity(preview_groups.len());
        let mut preview_rows_by_slot: Vec<[Vec<RowRef>; 2]> =
            Vec::with_capacity(preview_groups.len());
        for &group_idx in preview_groups.values() {
            let slot = preview_rows_by_slot.len();
            preview_group_slot.insert(group_idx, slot);
            preview_rows_by_slot.push([Vec::new(), Vec::new()]);
        }
        for side in 0..2 {
            let Some(file) = metas[side].file.as_ref() else {
                continue;
            };
            let mut reader = BufReader::with_capacity(READ_CHUNK, file.open()?);
            let mut seen_rows = 0_u64;
            let mut last_ordinal: Option<u64> = None;
            let mut observed_fp = LocatorFingerprint::default();
            while let Some(head) = read_head(&mut reader, config, state)? {
                state.check()?;
                if last_ordinal.is_some_and(|last| head.ordinal <= last) {
                    return Err(error("Private join ordinals are out of order.").into());
                }
                let raw =
                    read_exact_fixed::<ROW_LOCATOR_BYTES>(&mut reader, head.record_len, state)?;
                let locator = RowLocator::from_le_bytes(&raw);
                validate_row_locator(locator, &inputs[side], head.ordinal, config)?;
                observed_fp.observe_row(head.ordinal, locator, &head.key);
                if let Some(group_idx) = table.find(&head.key)
                    && let Some(&slot) = preview_group_slot.get(&group_idx)
                    && preview_rows_by_slot[slot][side].len() < config.preview_rows
                {
                    preview_rows_by_slot[slot][side].push(RowRef {
                        ordinal: head.ordinal,
                        source_offset: locator.source_offset,
                        record_len: locator.record_len,
                    });
                }
                last_ordinal = Some(head.ordinal);
                seen_rows = add(seen_rows, 1)?;
            }
            if seen_rows != metas[side].rows || observed_fp != metas[side].fingerprint {
                return Err(error("Private join row locators changed.").into());
            }
        }
        for &group_idx in preview_groups.values() {
            let Some(&slot) = preview_group_slot.get(&group_idx) else {
                continue;
            };
            let group = &table.groups[group_idx];
            let key_digest = KeyDigest::from_bytes(table.key_bytes(group));
            let (left_rows, right_rows) = (
                &preview_rows_by_slot[slot][0],
                &preview_rows_by_slot[slot][1],
            );
            match (left_rows.is_empty(), right_rows.is_empty()) {
                (false, false) => {
                    let right_drives = config.mode == JoinMode::Right;
                    let mut emitted = 0_usize;
                    if right_drives {
                        'outer_r: for &r in right_rows {
                            for &l in left_rows {
                                let order =
                                    pair_order(config.mode, Some(l.ordinal), Some(r.ordinal));
                                insert_bounded_preview(
                                    &mut out.preview_candidates,
                                    order,
                                    PreviewCandidate {
                                        main_row: Some(l),
                                        additional_row: Some(r),
                                        left_key: Some(key_digest),
                                        right_key: Some(key_digest),
                                    },
                                    config.preview_rows,
                                );
                                emitted += 1;
                                if emitted >= config.preview_rows {
                                    break 'outer_r;
                                }
                            }
                        }
                    } else {
                        'outer_l: for &l in left_rows {
                            for &r in right_rows {
                                let order =
                                    pair_order(config.mode, Some(l.ordinal), Some(r.ordinal));
                                insert_bounded_preview(
                                    &mut out.preview_candidates,
                                    order,
                                    PreviewCandidate {
                                        main_row: Some(l),
                                        additional_row: Some(r),
                                        left_key: Some(key_digest),
                                        right_key: Some(key_digest),
                                    },
                                    config.preview_rows,
                                );
                                emitted += 1;
                                if emitted >= config.preview_rows {
                                    break 'outer_l;
                                }
                            }
                        }
                    }
                }
                (false, true) => {
                    for &l in left_rows.iter().take(config.preview_rows) {
                        let order = pair_order(config.mode, Some(l.ordinal), None);
                        insert_bounded_preview(
                            &mut out.preview_candidates,
                            order,
                            PreviewCandidate {
                                main_row: Some(l),
                                additional_row: None,
                                left_key: Some(key_digest),
                                right_key: None,
                            },
                            config.preview_rows,
                        );
                    }
                }
                (true, false) => {
                    for &r in right_rows.iter().take(config.preview_rows) {
                        let order = pair_order(config.mode, None, Some(r.ordinal));
                        insert_bounded_preview(
                            &mut out.preview_candidates,
                            order,
                            PreviewCandidate {
                                main_row: None,
                                additional_row: Some(r),
                                left_key: None,
                                right_key: Some(key_digest),
                            },
                            config.preview_rows,
                        );
                    }
                }
                (true, true) => {}
            }
        }
    }

    // Decide whether creation should automatically fall back to sort-merge for this partition.
    let est_build = est_build_0.min(est_build_1);
    if est_build > create_mem_cap || out.plan.max_group_pairs > SKEW_FALLBACK_PAIR_THRESHOLD {
        out.plan.use_sort_merge = true;
        out.plan.fallback_extra_bytes = mul(add(metas[0].bytes, metas[1].bytes)?, 2)?;
    }

    Ok(Some(out))
}

struct AnalysisAccumulator {
    partition_plans: Vec<Option<PartitionPlan>>,
    completed: usize,
    matching: [u64; 2],
    pairs: u64,
    unmatched: [u64; 2],
    duplicate_groups: [u64; 2],
    many_to_many_groups: u64,
    many_to_many_pairs: u64,
    output_rows: u64,
    output_bytes: u64,
    duplicate_samples: BTreeMap<SampleKey, JoinDuplicateExample>,
    unmatched_samples: [BTreeMap<SampleKey, JoinUnmatchedExample>; 2],
    preview_candidates: BTreeMap<(u64, u64), PreviewCandidate>,
    earliest_unique: Option<(usize, u64)>,
    first_error: Option<Failure>,
}

impl AnalysisAccumulator {
    fn new(partition_count: usize, header_bytes: u64) -> Self {
        Self {
            partition_plans: vec![None; partition_count],
            completed: 0,
            matching: [0, 0],
            pairs: 0,
            unmatched: [0, 0],
            duplicate_groups: [0, 0],
            many_to_many_groups: 0,
            many_to_many_pairs: 0,
            output_rows: 0,
            output_bytes: header_bytes,
            duplicate_samples: BTreeMap::new(),
            unmatched_samples: [BTreeMap::new(), BTreeMap::new()],
            preview_candidates: BTreeMap::new(),
            earliest_unique: None,
            first_error: None,
        }
    }

    fn fold(&mut self, partition: usize, res: JResult<PartitionAnalysisOut>, preview_rows: usize) {
        self.completed += 1;
        let part = match res {
            Ok(part) => part,
            Err(err) => {
                if self.first_error.is_none() {
                    self.first_error = Some(err);
                }
                return;
            }
        };
        if let Some(u) = part.unique_error {
            if self.earliest_unique.is_none_or(|prev| u < prev) {
                self.earliest_unique = Some(u);
            }
            return;
        }
        self.partition_plans[partition] = Some(part.plan);
        let merge_counts = (|| -> JResult<()> {
            self.matching[0] = add(self.matching[0], part.matching[0])?;
            self.matching[1] = add(self.matching[1], part.matching[1])?;
            self.pairs = add(self.pairs, part.pairs)?;
            self.unmatched[0] = add(self.unmatched[0], part.unmatched[0])?;
            self.unmatched[1] = add(self.unmatched[1], part.unmatched[1])?;
            self.duplicate_groups[0] = add(self.duplicate_groups[0], part.duplicate_groups[0])?;
            self.duplicate_groups[1] = add(self.duplicate_groups[1], part.duplicate_groups[1])?;
            self.many_to_many_groups = add(self.many_to_many_groups, part.many_to_many_groups)?;
            self.many_to_many_pairs = add(self.many_to_many_pairs, part.many_to_many_pairs)?;
            self.output_rows = add(self.output_rows, part.output_rows)?;
            self.output_bytes = add(self.output_bytes, part.output_bytes)?;
            Ok(())
        })();
        if let Err(err) = merge_counts {
            if self.first_error.is_none() {
                self.first_error = Some(err);
            }
            return;
        }
        for (k, v) in part.duplicate_samples {
            insert_bounded_sample(&mut self.duplicate_samples, k, v, MAX_GROUP_EXAMPLES);
        }
        let [u0, u1] = part.unmatched_samples;
        for (k, v) in u0 {
            insert_bounded_sample(&mut self.unmatched_samples[0], k, v, MAX_GROUP_EXAMPLES);
        }
        for (k, v) in u1 {
            insert_bounded_sample(&mut self.unmatched_samples[1], k, v, MAX_GROUP_EXAMPLES);
        }
        for (order, cand) in part.preview_candidates {
            insert_bounded_preview(&mut self.preview_candidates, order, cand, preview_rows);
        }
    }
}

fn fallback_sort_config(config: JoinConfig, hash_budget_total: usize) -> JoinConfig {
    JoinConfig {
        memory_bytes: config.memory_bytes.saturating_sub(hash_budget_total),
        ..config
    }
}

/// Extra bytes a fallback sort-merge must reserve from `JobByteBudget` when `max_key` is so large
/// that even a 2-way merge exceeds the non-hash slice of `config.memory_bytes`.
fn fallback_extra_budget_bytes(
    config: JoinConfig,
    hash_budget_total: usize,
    max_key: usize,
) -> usize {
    let fallback_mem = config.memory_bytes.saturating_sub(hash_budget_total);
    let reserved = config
        .max_record_bytes
        .saturating_mul(3)
        .saturating_add(config.run_memory_bytes)
        .saturating_add(config.group_memory_bytes)
        .saturating_add(config.preview_bytes)
        .saturating_add(GROUP_EXAMPLE_RESERVE)
        .saturating_add(config.max_columns.saturating_mul(64))
        .saturating_add(4 * 1024 * 1024);
    let head = max_key.saturating_add(8192 + 64);
    let min_two_way_need = head.saturating_mul(3);
    let available = fallback_mem.saturating_sub(reserved);
    min_two_way_need
        .saturating_sub(available)
        .min(hash_budget_total)
}

fn analyze_partitioned(
    inputs: &[PreparedInput; 2],
    sides: [PartitionedInput; 2],
    partition_count: usize,
    output: &OutputLayout,
    workspace: &Arc<SpillWorkspace>,
    config: JoinConfig,
    state: &Arc<Progress>,
) -> JResult<(HashJoinPlan, JoinAnalysis)> {
    state.phase(JoinPhase::Analyzing);
    let header_bytes = output_header(output, inputs[0].info.delimiter, config)?.len() as u64;
    let columns = output.columns.len() as u64;

    let (hash_budget_total, per_partition_cap) = hash_budget_limits(config);
    let fallback_config = fallback_sort_config(config, hash_budget_total);
    let max_part_est = (0..partition_count)
        .map(|p| estimate_analysis_ram(&sides[0].partitions[p], &sides[1].partitions[p]))
        .max()
        .unwrap_or(0)
        .min(per_partition_cap);
    let max_concurrent_tables = hash_budget_total
        .checked_div(max_part_est)
        .map_or(MAX_HASH_WORKERS, |tables| tables.max(1));
    let budget = Arc::new(JobByteBudget::new(hash_budget_total));
    let fallback_lock: Arc<Mutex<Option<Failure>>> = Arc::new(Mutex::new(None));
    let next_part = Arc::new(AtomicUsize::new(0));
    let accumulator = Arc::new(Mutex::new(AnalysisAccumulator::new(
        partition_count,
        header_bytes,
    )));
    let workers = default_worker_threads()
        .clamp(1, MAX_HASH_WORKERS)
        .min(partition_count)
        .min(max_concurrent_tables);
    #[cfg(test)]
    let workers = storage_test::force_hash_workers().map_or(workers, |forced| {
        forced.clamp(1, MAX_HASH_WORKERS).min(partition_count)
    });

    // Share read-only slices via raw-free Arc wrappers built from cloned lightweight metadata.
    let shared_inputs = Arc::new([
        PreparedInput {
            info: temporary_info_clone(&inputs[0].info),
            physical_path: inputs[0].physical_path.clone(),
            working: None,
            stamp: inputs[0].stamp.clone(),
            storage: inputs[0].storage,
            locator_fingerprint: inputs[0].locator_fingerprint,
            key_columns: inputs[0].key_columns.clone(),
            require_unique: inputs[0].require_unique,
            output_delimiter: inputs[0].output_delimiter,
            projection: Vec::new(),
            sorted: None,
            frame_bytes: inputs[0].frame_bytes,
        },
        PreparedInput {
            info: temporary_info_clone(&inputs[1].info),
            physical_path: inputs[1].physical_path.clone(),
            working: None,
            stamp: inputs[1].stamp.clone(),
            storage: inputs[1].storage,
            locator_fingerprint: inputs[1].locator_fingerprint,
            key_columns: inputs[1].key_columns.clone(),
            require_unique: inputs[1].require_unique,
            output_delimiter: inputs[1].output_delimiter,
            projection: Vec::new(),
            sorted: None,
            frame_bytes: inputs[1].frame_bytes,
        },
    ]);
    let shared_sides = Arc::new(sides);
    let ws = Arc::clone(workspace);
    let st = Arc::clone(state);
    let b_arc = Arc::clone(&budget);
    let fb_arc = Arc::clone(&fallback_lock);
    let np_arc = Arc::clone(&next_part);
    let acc_arc = Arc::clone(&accumulator);
    let in_arc = Arc::clone(&shared_inputs);
    let sd_arc = Arc::clone(&shared_sides);

    run_on_engine_pool(workers, move |_| {
        let mut table = AnalysisTable::new();
        let mut held_permit: Option<JobBytePermit<'_>> = None;
        loop {
            if st.check().is_err() {
                break;
            }
            let p = np_arc.fetch_add(1, Ordering::AcqRel);
            if p >= partition_count {
                break;
            }
            let metas = [&sd_arc[0].partitions[p], &sd_arc[1].partitions[p]];
            let out: JResult<PartitionAnalysisOut> = (|| {
                if metas[0].rows == 0 && metas[1].rows == 0 {
                    return Ok(PartitionAnalysisOut::empty(PartitionPlan {
                        build_side: 0,
                        use_sort_merge: false,
                        max_group_pairs: 0,
                        fallback_extra_bytes: 0,
                    }));
                }
                #[cfg(test)]
                storage_test::record_hash_partition();

                let est = estimate_analysis_ram(metas[0], metas[1]).min(per_partition_cap);
                let need = est.max(1);
                // Keep retained table capacity charged to `JobByteBudget` at all times; release
                // both `table` and `held_permit` before acquiring a larger permit so we never
                // hold uncharged memory or deadlock under backpressure.
                let permit_bytes = match held_permit.as_ref() {
                    Some(permit) if permit.bytes >= need => permit.bytes,
                    _ => {
                        table.reset(0);
                        held_permit = None;
                        let permit = b_arc.acquire(need, &st)?;
                        let bytes = permit.bytes;
                        held_permit = Some(permit);
                        bytes
                    }
                };
                let effective_cap = permit_bytes.min(per_partition_cap);
                if let Some(analyzed) = analyze_partition_hash(
                    &in_arc,
                    metas,
                    columns,
                    &mut table,
                    effective_cap,
                    per_partition_cap,
                    config,
                    &st,
                )? {
                    if table.mem_bytes() > permit_bytes {
                        table.reset(0);
                    }
                    st.records.fetch_add(metas[0].rows, Ordering::AcqRel);
                    return Ok(analyzed);
                }
                // Exceeded budget dynamically; release table and permit before sort-merge fallback.
                table.reset(0);
                held_permit = None;
                let mut fb_fatal = fb_arc.lock().unwrap();
                if fb_fatal.is_some() {
                    return Err(Failure::Cancelled);
                }
                let max_key = metas[0].max_key_bytes.max(metas[1].max_key_bytes).max(16);
                let extra_budget = fallback_extra_budget_bytes(config, hash_budget_total, max_key);
                let _fb_permit = if extra_budget > 0 {
                    Some(b_arc.acquire(extra_budget, &st)?)
                } else {
                    None
                };
                match analyze_partition_sort_merge(
                    &in_arc,
                    metas,
                    columns,
                    &ws,
                    fallback_config,
                    &st,
                ) {
                    Ok(analyzed) => {
                        st.records.fetch_add(metas[0].rows, Ordering::AcqRel);
                        Ok(analyzed)
                    }
                    Err(Failure::Cancelled) => Err(Failure::Cancelled),
                    Err(err) => {
                        // Record the fatal fallback/cleanup failure before releasing `fallback_lock`
                        // so no later worker can start another fallback beyond the single-fallback
                        // disk allowance.
                        *fb_fatal = Some(err);
                        Err(Failure::Cancelled)
                    }
                }
            })();
            if out.is_err() {
                table.reset(0);
                held_permit = None;
            }
            acc_arc.lock().unwrap().fold(p, out, config.preview_rows);
        }
        drop(table);
        drop(held_permit);
    })?;

    state.check()?;
    let sides = Arc::try_unwrap(shared_sides).expect("analysis workers finished");
    drop(shared_inputs);

    let acc = Arc::try_unwrap(accumulator)
        .ok()
        .expect("analysis accumulator unshared")
        .into_inner()
        .unwrap();

    if let Some((side, record)) = acc.earliest_unique {
        return Err(input_error(
            &inputs[side].info.path,
            Some(record),
            "Repeated key found, but Require unique key is enabled for this input.",
        )
        .into());
    }
    if let Some(err) = fallback_lock.lock().unwrap().take() {
        return Err(err);
    }
    if let Some(err) = acc.first_error {
        return Err(err);
    }
    if acc.completed != partition_count {
        return Err(Failure::Cancelled);
    }

    let partition_plans: Vec<PartitionPlan> = acc
        .partition_plans
        .into_iter()
        .map(|slot| slot.ok_or(Failure::Cancelled))
        .collect::<JResult<_>>()?;

    // Creation-time fallbacks are serialized under `fallback_lock`, so reserve the single largest
    // partition's worst-case fallback scratch (2x both sides) across ALL partitions, covering
    // both planned and dynamic creation-time fallbacks.
    let mut max_fallback_extra = 0_u64;
    for p in 0..partition_count {
        let part_bytes = add(sides[0].partitions[p].bytes, sides[1].partitions[p].bytes)?;
        let fallback_bytes = mul(part_bytes, 2)?;
        max_fallback_extra = max_fallback_extra.max(fallback_bytes);
    }

    let matching = acc.matching;
    let pairs = acc.pairs;
    let mut unmatched = acc.unmatched;
    let duplicate_groups = acc.duplicate_groups;
    let many_to_many_groups = acc.many_to_many_groups;
    let many_to_many_pairs = acc.many_to_many_pairs;
    let mut output_rows = acc.output_rows;
    let mut output_bytes = acc.output_bytes;
    let duplicate_samples_map = acc.duplicate_samples;
    let unmatched_matchable_maps = acc.unmatched_samples;
    let mut preview_candidates = acc.preview_candidates;

    // Account for empty/missing-key streams (`key[0] == 0`), which sort before all `key[0] == 1`
    // unmatched samples and are never matched.
    let mut unmatched_samples: [Vec<JoinUnmatchedExample>; 2] =
        std::array::from_fn(|_| Vec::with_capacity(MAX_GROUP_EXAMPLES));
    for side in 0..2 {
        let empty = &sides[side].empty;
        if empty.rows > 0 {
            unmatched[side] = add(unmatched[side], empty.rows)?;
            let retained = config.mode.retains(side == 0, side == 1);
            if retained {
                output_rows = add(output_rows, empty.rows)?;
                output_bytes = add(
                    output_bytes,
                    add(empty.value_bytes, mul(empty.rows, columns)?)?,
                )?;
                if empty.max_value_bytes.saturating_add(columns as usize) > config.max_record_bytes
                {
                    return Err(error("A joined record exceeds the configured size limit.").into());
                }
            }
            for row in empty.first_rows.iter().take(MAX_GROUP_EXAMPLES) {
                unmatched_samples[side].push(JoinUnmatchedExample {
                    data_row: row.ordinal + 1,
                    key: None,
                });
            }
            if retained && config.preview_rows > 0 {
                for &row in empty.first_rows.iter().take(config.preview_rows) {
                    let order = pair_order(
                        config.mode,
                        (side == 0).then_some(row.ordinal),
                        (side == 1).then_some(row.ordinal),
                    );
                    let empty_digest = KeyDigest::from_empty_ordinal(row.ordinal);
                    insert_bounded_preview(
                        &mut preview_candidates,
                        order,
                        PreviewCandidate {
                            main_row: (side == 0).then_some(row),
                            additional_row: (side == 1).then_some(row),
                            left_key: (side == 0).then_some(empty_digest),
                            right_key: (side == 1).then_some(empty_digest),
                        },
                        config.preview_rows,
                    );
                }
            }
        }
        let remaining = MAX_GROUP_EXAMPLES.saturating_sub(unmatched_samples[side].len());
        unmatched_samples[side].extend(
            unmatched_matchable_maps[side]
                .values()
                .take(remaining)
                .cloned(),
        );
    }

    // Materialize ONLY the final top `<= config.preview_rows` preview rows.
    let mut preview_files: [Option<File>; 2] = [None, None];
    let mut preview_buf = Vec::new();
    let mut preview = Vec::with_capacity(preview_candidates.len());
    for (_, cand) in preview_candidates {
        let mut load = |side: usize,
                        row: Option<RowRef>,
                        expected_digest: Option<KeyDigest>|
         -> JResult<Option<Vec<u8>>> {
            let Some(row) = row else {
                return Ok(None);
            };
            let role = if side == 0 {
                JoinSource::Main
            } else {
                JoinSource::Additional
            };
            let file = match &preview_files[side] {
                Some(file) => file,
                None => preview_files[side].insert(check_source(
                    &inputs[side],
                    role,
                    JoinPhase::Analyzing,
                )?),
            };
            let record = materialize_seekable_record(
                file,
                &inputs[side],
                role,
                JoinPhase::Analyzing,
                row,
                config,
                state,
            )?;
            if let Some(expected_digest) = expected_digest {
                let parsed = fields(
                    &record,
                    inputs[side].info.delimiter,
                    false,
                    &inputs[side].info.path,
                    row.ordinal + 1,
                    config,
                )?;
                let observed = KeyDigest::from_fields(
                    &parsed,
                    &inputs[side].key_columns,
                    row.ordinal,
                    config.case,
                );
                if observed != expected_digest {
                    return Err(error("Private join key differs from its record.").into());
                }
            }
            Ok(Some(record))
        };
        let left_rec = load(0, cand.main_row, cand.left_key)?;
        let right_rec = load(1, cand.additional_row, cand.right_key)?;
        if let (Some(l_rec), Some(r_rec)) = (left_rec.as_deref(), right_rec.as_deref()) {
            let l_parsed = fields(
                l_rec,
                inputs[0].info.delimiter,
                false,
                &inputs[0].info.path,
                cand.main_row.map_or(0, |r| r.ordinal + 1),
                config,
            )?;
            let r_parsed = fields(
                r_rec,
                inputs[1].info.delimiter,
                false,
                &inputs[1].info.path,
                cand.additional_row.map_or(0, |r| r.ordinal + 1),
                config,
            )?;
            if !fields_keys_match(
                &l_parsed,
                &inputs[0].key_columns,
                &r_parsed,
                &inputs[1].key_columns,
                config.case,
            ) {
                return Err(error("Private join key differs from its record.").into());
            }
        }
        output_record_into(
            inputs,
            left_rec.as_deref(),
            right_rec.as_deref(),
            output,
            config,
            &mut preview_buf,
        )?;
        preview.push(preview_row(
            &preview_buf,
            inputs[0].info.delimiter,
            config.preview_bytes / config.preview_rows.max(1),
            config,
        )?);
    }

    let frames = mul(output_rows, PAIR_LOCATOR_FRAME_BYTES)?;
    let spill_bytes = add(mul(frames, 2)?, max_fallback_extra)?;
    let temporary = add(spill_bytes, output_bytes)?;
    for (role, input) in [JoinSource::Main, JoinSource::Additional]
        .into_iter()
        .zip(inputs)
    {
        check_source(input, role, JoinPhase::Analyzing)?;
    }

    let hash_plan = HashJoinPlan {
        partition_count,
        sides,
        partition_plans,
        max_fallback_extra_bytes: max_fallback_extra,
    };
    let analysis = JoinAnalysis {
        input_rows: [inputs[0].info.data_rows, inputs[1].info.data_rows],
        matching_rows: matching,
        matching_pairs: pairs,
        unmatched_rows: unmatched,
        unmatched_samples,
        empty_key_rows: [inputs[0].info.empty_key_rows, inputs[1].info.empty_key_rows],
        duplicate_groups,
        many_to_many_groups,
        many_to_many_pairs,
        duplicate_samples: duplicate_samples_map.into_values().collect(),
        output_rows,
        output_bytes,
        additional_temporary_bytes: temporary,
        preview,
        output_headers: Arc::clone(&output.headers),
        include_header: output.include_header,
        duplicate_headers: output.duplicate_headers,
        new_columns: output.new_columns,
    };
    Ok((hash_plan, analysis))
}

#[derive(Clone, Copy)]
struct BuildRow {
    ordinal: u64,
    source_offset: u64,
    record_len: u32,
    next_row: u32,
}

#[derive(Clone, Copy)]
struct BuildBucket {
    hash: u64,
    key_start: u32,
    /// Low 31 bits = key length; bit 31 = `matched`.
    key_len_and_matched: u32,
    first_row: u32,
    last_row: u32,
}

impl BuildBucket {
    #[inline]
    fn key_len(self) -> usize {
        (self.key_len_and_matched & KEY_LEN_MASK) as usize
    }

    #[inline]
    fn is_matched(self) -> bool {
        (self.key_len_and_matched & MATCHED_BIT) != 0
    }

    #[inline]
    fn mark_matched(&mut self) {
        self.key_len_and_matched |= MATCHED_BIT;
    }
}

/// Reusable compact open-addressed hash table for creation-phase partition probing.
struct BuildTable {
    key_arena: Vec<u8>,
    rows: Vec<BuildRow>,
    buckets: Vec<BuildBucket>,
    slots: Vec<u32>,
}

impl BuildTable {
    fn new() -> Self {
        Self {
            key_arena: Vec::new(),
            rows: Vec::new(),
            buckets: Vec::new(),
            slots: Vec::new(),
        }
    }

    fn reset(&mut self, max_retain_bytes: usize) {
        self.key_arena.clear();
        self.rows.clear();
        self.buckets.clear();
        self.slots.fill(EMPTY_SLOT);
        if self.mem_bytes() > max_retain_bytes {
            self.key_arena = Vec::new();
            self.rows = Vec::new();
            self.buckets = Vec::new();
            self.slots = Vec::new();
        }
    }

    fn mem_bytes(&self) -> usize {
        self.key_arena
            .capacity()
            .saturating_add(
                self.rows
                    .capacity()
                    .saturating_mul(std::mem::size_of::<BuildRow>()),
            )
            .saturating_add(
                self.buckets
                    .capacity()
                    .saturating_mul(std::mem::size_of::<BuildBucket>()),
            )
            .saturating_add(
                self.slots
                    .capacity()
                    .saturating_mul(std::mem::size_of::<u32>()),
            )
    }

    fn prepare_for_meta(&mut self, meta: &PartitionMeta, mem_cap: usize) -> Result<(), ()> {
        self.reset(mem_cap);
        let est_rows = usize::try_from(meta.rows).map_err(|_| ())?;
        let est_keys = usize::try_from(meta.key_bytes).map_err(|_| ())?;
        let slot_len = (est_rows.saturating_mul(10) / 7 + 1)
            .next_power_of_two()
            .max(64);
        let projected = est_keys
            .saturating_add(est_rows.saturating_mul(std::mem::size_of::<BuildRow>()))
            .saturating_add(est_rows.saturating_mul(std::mem::size_of::<BuildBucket>()))
            .saturating_add(slot_len.saturating_mul(std::mem::size_of::<u32>()));
        if projected > mem_cap {
            self.reset(0);
            return Err(());
        }
        // Account for retained buffers from prior partitions BEFORE reserving any growth so
        // combined capacity never exceeds `mem_cap` even transiently.
        let combined = self
            .key_arena
            .capacity()
            .max(est_keys)
            .saturating_add(
                self.rows
                    .capacity()
                    .max(est_rows)
                    .saturating_mul(std::mem::size_of::<BuildRow>()),
            )
            .saturating_add(
                self.buckets
                    .capacity()
                    .max(est_rows)
                    .saturating_mul(std::mem::size_of::<BuildBucket>()),
            )
            .saturating_add(
                self.slots
                    .capacity()
                    .max(slot_len)
                    .saturating_mul(std::mem::size_of::<u32>()),
            );
        if combined > mem_cap {
            self.reset(0);
        }
        if self.key_arena.capacity() < est_keys {
            self.key_arena.try_reserve_exact(est_keys).map_err(|_| ())?;
        }
        if self.rows.capacity() < est_rows {
            self.rows.try_reserve_exact(est_rows).map_err(|_| ())?;
        }
        if self.buckets.capacity() < est_rows {
            self.buckets.try_reserve_exact(est_rows).map_err(|_| ())?;
        }
        if self.slots.len() < slot_len {
            self.slots = Vec::new();
            let mut new_slots = Vec::new();
            new_slots.try_reserve_exact(slot_len).map_err(|_| ())?;
            new_slots.resize(slot_len, EMPTY_SLOT);
            self.slots = new_slots;
        }
        if self.mem_bytes() > mem_cap {
            self.reset(0);
            return Err(());
        }
        Ok(())
    }

    fn insert(
        &mut self,
        key: &[u8],
        ordinal: u64,
        locator: RowLocator,
        mem_cap: usize,
    ) -> Result<(), ()> {
        let record_len = u32::try_from(locator.record_len).map_err(|_| ())?;
        let row_idx = u32::try_from(self.rows.len()).map_err(|_| ())?;
        if row_idx == NO_NEXT_ROW {
            return Err(());
        }
        let h = hash_key(key);
        let mask = self.slots.len() - 1;
        let mut pos = (h as usize) & mask;
        loop {
            let slot = self.slots[pos];
            if slot == EMPTY_SLOT {
                break;
            }
            let bucket = &mut self.buckets[(slot - 1) as usize];
            if bucket.hash == h
                && bucket.key_len() == key.len()
                && &self.key_arena[bucket.key_start as usize..bucket.key_start as usize + key.len()]
                    == key
            {
                let prev_last = bucket.last_row as usize;
                self.rows.push(BuildRow {
                    ordinal,
                    source_offset: locator.source_offset,
                    record_len,
                    next_row: NO_NEXT_ROW,
                });
                self.rows[prev_last].next_row = row_idx;
                bucket.last_row = row_idx;
                if self.mem_bytes() > mem_cap {
                    return Err(());
                }
                return Ok(());
            }
            pos = (pos + 1) & mask;
        }

        let key_start = u32::try_from(self.key_arena.len()).map_err(|_| ())?;
        let key_len_u32 = u32::try_from(key.len()).map_err(|_| ())?;
        if key_len_u32 > KEY_LEN_MASK {
            return Err(());
        }
        let bucket_idx = u32::try_from(self.buckets.len()).map_err(|_| ())?;
        if bucket_idx == u32::MAX {
            return Err(());
        }
        if (self.buckets.len() + 1).saturating_mul(10) > self.slots.len().saturating_mul(7) {
            let new_len = self.slots.len().saturating_mul(2);
            let mut new_slots = Vec::new();
            new_slots.try_reserve_exact(new_len).map_err(|_| ())?;
            new_slots.resize(new_len, EMPTY_SLOT);
            let new_mask = new_len - 1;
            for (idx, b) in self.buckets.iter().enumerate() {
                let mut p = (b.hash as usize) & new_mask;
                while new_slots[p] != EMPTY_SLOT {
                    p = (p + 1) & new_mask;
                }
                new_slots[p] = (idx + 1) as u32;
            }
            self.slots = new_slots;
            let new_mask = self.slots.len() - 1;
            pos = (h as usize) & new_mask;
            while self.slots[pos] != EMPTY_SLOT {
                pos = (pos + 1) & new_mask;
            }
        }
        self.key_arena.extend_from_slice(key);
        self.rows.push(BuildRow {
            ordinal,
            source_offset: locator.source_offset,
            record_len,
            next_row: NO_NEXT_ROW,
        });
        self.buckets.push(BuildBucket {
            hash: h,
            key_start,
            key_len_and_matched: key_len_u32,
            first_row: row_idx,
            last_row: row_idx,
        });
        self.slots[pos] = bucket_idx + 1;
        if self.mem_bytes() > mem_cap {
            return Err(());
        }
        Ok(())
    }

    #[inline]
    fn find_mut(&mut self, key: &[u8]) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let h = hash_key(key);
        let mask = self.slots.len() - 1;
        let mut pos = (h as usize) & mask;
        loop {
            let slot = self.slots[pos];
            if slot == EMPTY_SLOT {
                return None;
            }
            let idx = (slot - 1) as usize;
            let bucket = self.buckets[idx];
            if bucket.hash == h
                && bucket.key_len() == key.len()
                && &self.key_arena[bucket.key_start as usize..bucket.key_start as usize + key.len()]
                    == key
            {
                return Some(idx);
            }
            pos = (pos + 1) & mask;
        }
    }
}

struct PartitionCreateOut {
    run: Option<Run>,
    count: u64,
    pair_fingerprint: LocatorFingerprint,
}

fn unlink_partition_pair(metas: [&PartitionMeta; 2], state: &Progress) -> JResult<()> {
    for meta in metas {
        if let Some(file) = meta.file.as_ref() {
            file.remove()?;
            state.sub_disk(meta.bytes);
        }
    }
    Ok(())
}

/// Fast path when one side of a partition is empty and the non-empty side is retained: rows are
/// already in strictly ascending source ordinal order, so emit a single pre-sorted `PairLocator`
/// run without hashing or sorting.
fn create_partition_single_side_retained(
    input: &PreparedInput,
    side: usize,
    meta: &PartitionMeta,
    workspace: &SpillWorkspace,
    config: JoinConfig,
    state: &Progress,
) -> JResult<PartitionCreateOut> {
    let Some(file) = meta.file.as_ref() else {
        return Ok(PartitionCreateOut {
            run: None,
            count: 0,
            pair_fingerprint: LocatorFingerprint::default(),
        });
    };
    let mut reader = BufReader::with_capacity(READ_CHUNK, file.open()?);
    let (out_path, mut writer) = workspace.create_run()?;
    let mut seen_rows = 0_u64;
    let mut last_ordinal: Option<u64> = None;
    let mut observed_fp = LocatorFingerprint::default();
    let mut pair_fp = LocatorFingerprint::default();
    let mut bytes = 0_u64;

    while let Some(head) = read_head(&mut reader, config, state)? {
        state.check()?;
        if last_ordinal.is_some_and(|last| head.ordinal <= last) {
            return Err(error("Private join ordinals are out of order.").into());
        }
        let raw = read_exact_fixed::<ROW_LOCATOR_BYTES>(&mut reader, head.record_len, state)?;
        let locator = RowLocator::from_le_bytes(&raw);
        validate_row_locator(locator, input, head.ordinal, config)?;
        observed_fp.observe_row(head.ordinal, locator, &head.key);

        let order = pair_order(
            config.mode,
            (side == 0).then_some(head.ordinal),
            (side == 1).then_some(head.ordinal),
        );
        let pair_loc = if side == 0 {
            PairLocator {
                main_offset: locator.source_offset,
                main_len: locator.record_len,
                additional_offset: 0,
                additional_len: 0,
            }
        } else {
            PairLocator {
                main_offset: 0,
                main_len: 0,
                additional_offset: locator.source_offset,
                additional_len: locator.record_len,
            }
        };
        pair_fp.observe_pair(order.0, order.1, pair_loc);
        let pkey = pair_key(order);
        let ploc = pair_loc.to_le_bytes();
        let written = write_partition_frame(workspace, state, &mut writer, &pkey, &ploc, order.0)?;
        bytes = add(bytes, written)?;
        state.add_disk(written)?;
        last_ordinal = Some(head.ordinal);
        seen_rows = add(seen_rows, 1)?;
        state.records.fetch_add(1, Ordering::AcqRel);
    }
    flush_partition_writer(workspace, state, &mut writer)?;
    drop(writer);
    drop(reader);
    if seen_rows != meta.rows || observed_fp != meta.fingerprint {
        return cleanup_fallback_runs(
            Err(error("Private join row locators changed.").into()),
            &[(&out_path, bytes)],
            state,
        );
    }
    Ok(PartitionCreateOut {
        run: (bytes > 0).then_some(Run {
            path: out_path,
            bytes,
        }),
        count: seen_rows,
        pair_fingerprint: pair_fp,
    })
}

/// Convert a retained empty-key row stream (`key[0] == 0`, already in ascending source ordinal
/// order) into a pre-sorted `PairLocator` `Run` when `fan_in` is too narrow to keep all empty
/// streams open directly alongside `pair_runs`.
fn create_empty_pair_run(
    input: &PreparedInput,
    side: usize,
    empty: &EmptyPartitionMeta,
    workspace: &SpillWorkspace,
    config: JoinConfig,
    state: &Progress,
) -> JResult<Run> {
    let file = empty
        .file
        .as_ref()
        .ok_or_else(|| error("Missing private empty-key stream."))?;
    let mut reader = BufReader::with_capacity(READ_CHUNK, file.open()?);
    let (out_path, mut writer) = workspace.create_run()?;
    let mut seen_rows = 0_u64;
    let mut last_ordinal: Option<u64> = None;
    let mut observed_fp = LocatorFingerprint::default();
    let mut bytes = 0_u64;

    let build_res = (|| -> JResult<()> {
        while let Some(head) = read_head(&mut reader, config, state)? {
            state.check()?;
            if last_ordinal.is_some_and(|last| head.ordinal <= last) {
                return Err(error("Private join ordinals are out of order.").into());
            }
            let raw = read_exact_fixed::<ROW_LOCATOR_BYTES>(&mut reader, head.record_len, state)?;
            let locator = RowLocator::from_le_bytes(&raw);
            validate_row_locator(locator, input, head.ordinal, config)?;
            observed_fp.observe_row(head.ordinal, locator, &head.key);

            let order = pair_order(
                config.mode,
                (side == 0).then_some(head.ordinal),
                (side == 1).then_some(head.ordinal),
            );
            let pair_loc = if side == 0 {
                PairLocator {
                    main_offset: locator.source_offset,
                    main_len: locator.record_len,
                    additional_offset: 0,
                    additional_len: 0,
                }
            } else {
                PairLocator {
                    main_offset: 0,
                    main_len: 0,
                    additional_offset: locator.source_offset,
                    additional_len: locator.record_len,
                }
            };
            let pkey = pair_key(order);
            let ploc = pair_loc.to_le_bytes();
            let written =
                write_partition_frame(workspace, state, &mut writer, &pkey, &ploc, order.0)?;
            bytes = add(bytes, written)?;
            state.add_disk(written)?;
            last_ordinal = Some(head.ordinal);
            seen_rows = add(seen_rows, 1)?;
        }
        flush_partition_writer(workspace, state, &mut writer)?;
        Ok(())
    })();
    drop(writer);
    drop(reader);
    if let Err(err) = build_res {
        return cleanup_fallback_runs(Err(err), &[(&out_path, bytes)], state);
    }
    if seen_rows != empty.rows || observed_fp != empty.fingerprint {
        return cleanup_fallback_runs(
            Err(error("Private join row locators changed.").into()),
            &[(&out_path, bytes)],
            state,
        );
    }
    file.remove()?;
    state.sub_disk(empty.bytes);
    Ok(Run {
        path: out_path,
        bytes,
    })
}

/// Probe one partition using `BuildTable`; returns `Ok(None)` if `BuildTable` exceeded `mem_cap`
/// and must fall back to external sort-merge.
#[allow(clippy::too_many_arguments)]
fn create_partition_hash(
    inputs: &[PreparedInput; 2],
    metas: [&PartitionMeta; 2],
    build_side: usize,
    table: &mut BuildTable,
    mem_cap: usize,
    worker_run_config: JoinConfig,
    workspace: &SpillWorkspace,
    state: &Progress,
) -> JResult<Option<PartitionCreateOut>> {
    let probe_side = 1 - build_side;
    if table.prepare_for_meta(metas[build_side], mem_cap).is_err() {
        return Ok(None);
    }

    // 1. Build hash table on `build_side`.
    if let Some(file) = metas[build_side].file.as_ref() {
        let mut reader = BufReader::with_capacity(READ_CHUNK, file.open()?);
        let mut seen = 0_u64;
        let mut last_ord: Option<u64> = None;
        let mut fp = LocatorFingerprint::default();
        while let Some(head) = read_head(&mut reader, worker_run_config, state)? {
            state.check()?;
            if last_ord.is_some_and(|last| head.ordinal <= last) {
                return Err(error("Private join ordinals are out of order.").into());
            }
            let raw = read_exact_fixed::<ROW_LOCATOR_BYTES>(&mut reader, head.record_len, state)?;
            let locator = RowLocator::from_le_bytes(&raw);
            validate_row_locator(
                locator,
                &inputs[build_side],
                head.ordinal,
                worker_run_config,
            )?;
            fp.observe_row(head.ordinal, locator, &head.key);
            if table
                .insert(&head.key, head.ordinal, locator, mem_cap)
                .is_err()
            {
                table.reset(0);
                return Ok(None);
            }
            last_ord = Some(head.ordinal);
            seen = add(seen, 1)?;
        }
        if seen != metas[build_side].rows || fp != metas[build_side].fingerprint {
            return Err(error("Private join row locators changed.").into());
        }
    }

    // 2. Probe `probe_side` and emit sorted `PairLocator` runs.
    let mut runs = Runs::new(workspace, worker_run_config, state, 16);
    let mut count = 0_u64;
    let mut pair_fp = LocatorFingerprint::default();
    let probe_is_main = probe_side == 0;
    let build_is_main = build_side == 0;
    let retain_probe_unmatched = worker_run_config
        .mode
        .retains(probe_is_main, !probe_is_main);
    let retain_build_unmatched = worker_run_config
        .mode
        .retains(build_is_main, !build_is_main);

    if let Some(file) = metas[probe_side].file.as_ref() {
        let mut reader = BufReader::with_capacity(READ_CHUNK, file.open()?);
        let mut seen = 0_u64;
        let mut last_ord: Option<u64> = None;
        let mut fp = LocatorFingerprint::default();
        while let Some(head) = read_head(&mut reader, worker_run_config, state)? {
            state.check()?;
            if last_ord.is_some_and(|last| head.ordinal <= last) {
                return Err(error("Private join ordinals are out of order.").into());
            }
            let raw = read_exact_fixed::<ROW_LOCATOR_BYTES>(&mut reader, head.record_len, state)?;
            let locator = RowLocator::from_le_bytes(&raw);
            validate_row_locator(
                locator,
                &inputs[probe_side],
                head.ordinal,
                worker_run_config,
            )?;
            fp.observe_row(head.ordinal, locator, &head.key);

            if let Some(bucket_idx) = table.find_mut(&head.key) {
                #[cfg(test)]
                crate::join_faults::observe(crate::join_faults::Event::ExpandingPairs);
                table.buckets[bucket_idx].mark_matched();
                let mut cur = table.buckets[bucket_idx].first_row;
                while cur != NO_NEXT_ROW {
                    state.check()?;
                    let b = table.rows[cur as usize];
                    let (main_ord, add_ord, ploc) = if build_is_main {
                        (
                            b.ordinal,
                            head.ordinal,
                            PairLocator {
                                main_offset: b.source_offset,
                                main_len: u64::from(b.record_len),
                                additional_offset: locator.source_offset,
                                additional_len: locator.record_len,
                            },
                        )
                    } else {
                        (
                            head.ordinal,
                            b.ordinal,
                            PairLocator {
                                main_offset: locator.source_offset,
                                main_len: locator.record_len,
                                additional_offset: b.source_offset,
                                additional_len: u64::from(b.record_len),
                            },
                        )
                    };
                    let order = pair_order(worker_run_config.mode, Some(main_ord), Some(add_ord));
                    pair_fp.observe_pair(order.0, order.1, ploc);
                    let pkey = pair_key(order);
                    runs.push(&pkey, EntryPayload::Pair(ploc.to_le_bytes()), order.0)?;
                    count = add(count, 1)?;
                    cur = b.next_row;
                }
                state.records.fetch_add(1, Ordering::AcqRel);
            } else if retain_probe_unmatched {
                let (main_ord, add_ord, ploc) = if probe_is_main {
                    (
                        Some(head.ordinal),
                        None,
                        PairLocator {
                            main_offset: locator.source_offset,
                            main_len: locator.record_len,
                            additional_offset: 0,
                            additional_len: 0,
                        },
                    )
                } else {
                    (
                        None,
                        Some(head.ordinal),
                        PairLocator {
                            main_offset: 0,
                            main_len: 0,
                            additional_offset: locator.source_offset,
                            additional_len: locator.record_len,
                        },
                    )
                };
                let order = pair_order(worker_run_config.mode, main_ord, add_ord);
                pair_fp.observe_pair(order.0, order.1, ploc);
                let pkey = pair_key(order);
                runs.push(&pkey, EntryPayload::Pair(ploc.to_le_bytes()), order.0)?;
                count = add(count, 1)?;
                state.records.fetch_add(1, Ordering::AcqRel);
            }
            last_ord = Some(head.ordinal);
            seen = add(seen, 1)?;
        }
        if seen != metas[probe_side].rows || fp != metas[probe_side].fingerprint {
            return Err(error("Private join row locators changed.").into());
        }
    }

    // 3. Emit unmatched build-side rows if retained by join mode.
    if retain_build_unmatched {
        for bucket in &table.buckets {
            if bucket.is_matched() {
                continue;
            }
            let mut cur = bucket.first_row;
            while cur != NO_NEXT_ROW {
                state.check()?;
                let b = table.rows[cur as usize];
                let (main_ord, add_ord, ploc) = if build_is_main {
                    (
                        Some(b.ordinal),
                        None,
                        PairLocator {
                            main_offset: b.source_offset,
                            main_len: u64::from(b.record_len),
                            additional_offset: 0,
                            additional_len: 0,
                        },
                    )
                } else {
                    (
                        None,
                        Some(b.ordinal),
                        PairLocator {
                            main_offset: 0,
                            main_len: 0,
                            additional_offset: b.source_offset,
                            additional_len: u64::from(b.record_len),
                        },
                    )
                };
                let order = pair_order(worker_run_config.mode, main_ord, add_ord);
                pair_fp.observe_pair(order.0, order.1, ploc);
                let pkey = pair_key(order);
                runs.push(&pkey, EntryPayload::Pair(ploc.to_le_bytes()), order.0)?;
                count = add(count, 1)?;
                cur = b.next_row;
            }
        }
    }

    let ordered = runs.finish()?;
    let run = if ordered.bytes > 0 {
        Some(ordered)
    } else {
        ordered.path.remove()?;
        None
    };
    Ok(Some(PartitionCreateOut {
        run,
        count,
        pair_fingerprint: pair_fp,
    }))
}

/// Create one partition's sorted `PairLocator` run via ADR 0011 external sort-merge + bounded
/// duplicate blocks (skew or memory-cap fallback).
fn create_partition_sort_merge(
    inputs: &[PreparedInput; 2],
    metas: [&PartitionMeta; 2],
    workspace: &SpillWorkspace,
    config: JoinConfig,
    state: &Progress,
) -> JResult<PartitionCreateOut> {
    #[cfg(test)]
    storage_test::record_hash_fallback();

    let sorted0 = sort_partition_file(&inputs[0], metas[0], workspace, config, state)?;
    let sorted1 = match sort_partition_file(&inputs[1], metas[1], workspace, config, state) {
        Ok(run) => run,
        Err(err) => {
            return cleanup_fallback_runs(Err(err), &[(&sorted0.path, sorted0.bytes)], state);
        }
    };
    let s0_path = sorted0.path.clone();
    let s1_path = sorted1.path.clone();
    let s0_bytes = sorted0.bytes;
    let s1_bytes = sorted1.bytes;

    let result = (|| -> JResult<PartitionCreateOut> {
        let views = [
            temporary_prepared_view(&inputs[0], metas[0], sorted0.path),
            temporary_prepared_view(&inputs[1], metas[1], sorted1.path),
        ];
        let mut runs = Runs::new(workspace, config, state, 16);
        let mut count = 0_u64;
        let mut pair_fp = LocatorFingerprint::default();
        let mut range_readers: [Option<BufReader<File>>; 2] = [None, None];
        walk_partition_sorted(inputs, &views, metas, config, state, |left, right| {
            enumerate_pairs(
                &views,
                left.map(Group::into_data),
                right.map(Group::into_data),
                &mut range_readers,
                config,
                state,
                None,
                |left, right| {
                    let main_ref = left.map(|item| match item {
                        GroupItem::Ref(row) => *row,
                        GroupItem::Owned(_) => unreachable!(),
                    });
                    let add_ref = right.map(|item| match item {
                        GroupItem::Ref(row) => *row,
                        GroupItem::Owned(_) => unreachable!(),
                    });
                    count = add(count, 1)?;
                    let order = pair_order(
                        config.mode,
                        main_ref.map(|r| r.ordinal),
                        add_ref.map(|r| r.ordinal),
                    );
                    let locator = PairLocator {
                        main_offset: main_ref.map_or(0, |r| r.source_offset),
                        main_len: main_ref.map_or(0, |r| r.record_len),
                        additional_offset: add_ref.map_or(0, |r| r.source_offset),
                        additional_len: add_ref.map_or(0, |r| r.record_len),
                    };
                    pair_fp.observe_pair(order.0, order.1, locator);
                    let pkey = pair_key(order);
                    runs.push(&pkey, EntryPayload::Pair(locator.to_le_bytes()), order.0)?;
                    state.records.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                },
            )?;
            Ok(())
        })?;
        drop(range_readers);
        let ordered = runs.finish()?;
        let run = if ordered.bytes > 0 {
            Some(ordered)
        } else {
            ordered.path.remove()?;
            None
        };
        Ok(PartitionCreateOut {
            run,
            count,
            pair_fingerprint: pair_fp,
        })
    })();

    cleanup_fallback_runs(result, &[(&s0_path, s0_bytes), (&s1_path, s1_bytes)], state)
}

enum MergeStream {
    PairRun {
        reader: BufReader<File>,
        file: SpillFile,
        bytes: u64,
    },
    EmptyRows {
        reader: BufReader<File>,
        file: SpillFile,
        side: usize,
        bytes: u64,
        expected_rows: u64,
        expected_fp: LocatorFingerprint,
        seen_rows: u64,
        last_ordinal: Option<u64>,
        observed_fp: LocatorFingerprint,
    },
}

struct MergeHeapEntry {
    order: [u8; 16],
    pair: OrderedPair,
    stream: usize,
}

impl PartialEq for MergeHeapEntry {
    fn eq(&self, other: &Self) -> bool {
        (self.order, self.stream) == (other.order, other.stream)
    }
}
impl Eq for MergeHeapEntry {}
impl PartialOrd for MergeHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for MergeHeapEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        (other.order, other.stream).cmp(&(self.order, self.stream))
    }
}

fn read_next_from_merge_stream(
    stream: &mut MergeStream,
    inputs: &[PreparedInput; 2],
    config: JoinConfig,
    state: &Progress,
) -> JResult<Option<([u8; 16], OrderedPair)>> {
    let right_drives = config.mode == JoinMode::Right;
    match stream {
        MergeStream::PairRun { reader, .. } => {
            let Some(head) = read_head(reader, config, state)? else {
                return Ok(None);
            };
            if !valid_pair_order(
                &head.key,
                head.ordinal,
                config.mode,
                [inputs[0].info.data_rows, inputs[1].info.data_rows],
            ) {
                return Err(error("Joined row pairs are not in source order.").into());
            }
            let key_bytes: [u8; 16] = head.key.try_into().unwrap();
            let first = u64::from_be_bytes(key_bytes[0..8].try_into().unwrap());
            let second = u64::from_be_bytes(key_bytes[8..16].try_into().unwrap());
            let (main_ord, additional_ord) = if right_drives {
                (second, first)
            } else {
                (first, second)
            };
            let main_ordinal = (main_ord != u64::MAX).then_some(main_ord);
            let additional_ordinal = (additional_ord != u64::MAX).then_some(additional_ord);
            let raw_locator =
                read_exact_fixed::<PAIR_LOCATOR_BYTES>(reader, head.record_len, state)?;
            let locator = PairLocator::from_le_bytes(&raw_locator);
            if main_ordinal.is_none() && (locator.main_offset != 0 || locator.main_len != 0)
                || additional_ordinal.is_none()
                    && (locator.additional_offset != 0 || locator.additional_len != 0)
            {
                return Err(error("Invalid private join pair locator.").into());
            }
            if main_ordinal.is_some()
                && (add(locator.main_offset, locator.main_len)? > inputs[0].stamp.file_size()
                    || locator.main_len > config.max_record_bytes as u64)
            {
                return Err(error("Invalid private join row locator.").into());
            }
            if additional_ordinal.is_some()
                && (add(locator.additional_offset, locator.additional_len)?
                    > inputs[1].stamp.file_size()
                    || locator.additional_len > config.max_record_bytes as u64)
            {
                return Err(error("Invalid private join row locator.").into());
            }
            Ok(Some((
                key_bytes,
                OrderedPair {
                    main_ordinal,
                    additional_ordinal,
                    locator,
                },
            )))
        }
        MergeStream::EmptyRows {
            reader,
            side,
            expected_rows,
            expected_fp,
            seen_rows,
            last_ordinal,
            observed_fp,
            ..
        } => {
            let Some(head) = read_head(reader, config, state)? else {
                if *seen_rows != *expected_rows || *observed_fp != *expected_fp {
                    return Err(error("Private join row locators changed.").into());
                }
                return Ok(None);
            };
            if last_ordinal.is_some_and(|last| head.ordinal <= last) {
                return Err(error("Private join ordinals are out of order.").into());
            }
            let raw = read_exact_fixed::<ROW_LOCATOR_BYTES>(reader, head.record_len, state)?;
            let locator = RowLocator::from_le_bytes(&raw);
            validate_row_locator(locator, &inputs[*side], head.ordinal, config)?;
            observed_fp.observe_row(head.ordinal, locator, &head.key);
            *last_ordinal = Some(head.ordinal);
            *seen_rows = add(*seen_rows, 1)?;
            let main_ordinal = (*side == 0).then_some(head.ordinal);
            let additional_ordinal = (*side == 1).then_some(head.ordinal);
            let order = pair_order(config.mode, main_ordinal, additional_ordinal);
            let key_bytes = pair_key(order);
            let ploc = if *side == 0 {
                PairLocator {
                    main_offset: locator.source_offset,
                    main_len: locator.record_len,
                    additional_offset: 0,
                    additional_len: 0,
                }
            } else {
                PairLocator {
                    main_offset: 0,
                    main_len: 0,
                    additional_offset: locator.source_offset,
                    additional_len: locator.record_len,
                }
            };
            Ok(Some((
                key_bytes,
                OrderedPair {
                    main_ordinal,
                    additional_ordinal,
                    locator: ploc,
                },
            )))
        }
    }
}

/// Execute parallel partitioned creation and streaming `K`-way ordinal merge directly into
/// `JoinPlan::materialize_seekable_pairs`.
pub(super) fn write_partitioned(
    plan: &JoinPlan,
    hash_plan: &HashJoinPlan,
    workspace: &Arc<SpillWorkspace>,
    source_files: &[File; 2],
    output: &mut ExportTarget,
    state: &Arc<Progress>,
) -> JResult<()> {
    let config = plan.config;
    let partition_count = hash_plan.partition_count;

    // Promptly unlink any unretained empty-key stream files before probing starts.
    let mut expected_pair_fp = LocatorFingerprint::default();
    let mut total_count = 0_u64;
    let mut retained_empty_sides: Vec<usize> = Vec::new();
    for (side, side_plan) in hash_plan.sides.iter().enumerate() {
        let empty = &side_plan.empty;
        if empty.rows == 0 {
            continue;
        }
        if config.mode.retains(side == 0, side == 1) {
            expected_pair_fp.combine(empty.pair_fingerprint);
            total_count = add(total_count, empty.rows)?;
            retained_empty_sides.push(side);
        } else if let Some(file) = empty.file.as_ref() {
            file.remove()?;
            state.sub_disk(empty.bytes);
        }
    }

    let (hash_budget_total, per_partition_cap) = hash_budget_limits(config);
    let fallback_config = fallback_sort_config(config, hash_budget_total);
    let max_build_est = (0..partition_count)
        .filter(|&p| !hash_plan.partition_plans[p].use_sort_merge)
        .map(|p| {
            let b = hash_plan.partition_plans[p].build_side;
            estimate_build_ram(&hash_plan.sides[b].partitions[p])
        })
        .max()
        .unwrap_or(0)
        .min(per_partition_cap);
    let max_concurrent_tables = hash_budget_total
        .checked_div(max_build_est)
        .map_or(MAX_HASH_WORKERS, |tables| tables.max(1));
    let budget = Arc::new(JobByteBudget::new(hash_budget_total));
    let fallback_lock: Arc<Mutex<Option<Failure>>> = Arc::new(Mutex::new(None));
    let next_part = Arc::new(AtomicUsize::new(0));
    let results: Arc<Mutex<Vec<Option<JResult<PartitionCreateOut>>>>> =
        Arc::new(Mutex::new((0..partition_count).map(|_| None).collect()));

    let workers = default_worker_threads()
        .clamp(1, MAX_HASH_WORKERS)
        .min(partition_count)
        .min(max_concurrent_tables);
    let worker_run_mem = (config.run_memory_bytes / workers.max(1)).max(MIN_HASH_RUN_BYTES);
    let fd_cap = engine_pool().fd_budget().limit();
    let worker_max_fan_in = (fd_cap / workers.max(1))
        .saturating_sub(3)
        .clamp(2, config.max_merge_fan_in);
    let worker_run_config = JoinConfig {
        run_memory_bytes: worker_run_mem.min(config.run_memory_bytes),
        max_merge_fan_in: worker_max_fan_in,
        ..config
    };

    let shared_inputs = Arc::new([
        PreparedInput {
            info: temporary_info_clone(&plan.inputs[0].info),
            physical_path: plan.inputs[0].physical_path.clone(),
            working: None,
            stamp: plan.inputs[0].stamp.clone(),
            storage: plan.inputs[0].storage,
            locator_fingerprint: plan.inputs[0].locator_fingerprint,
            key_columns: plan.inputs[0].key_columns.clone(),
            require_unique: plan.inputs[0].require_unique,
            output_delimiter: plan.inputs[0].output_delimiter,
            projection: Vec::new(),
            sorted: None,
            frame_bytes: plan.inputs[0].frame_bytes,
        },
        PreparedInput {
            info: temporary_info_clone(&plan.inputs[1].info),
            physical_path: plan.inputs[1].physical_path.clone(),
            working: None,
            stamp: plan.inputs[1].stamp.clone(),
            storage: plan.inputs[1].storage,
            locator_fingerprint: plan.inputs[1].locator_fingerprint,
            key_columns: plan.inputs[1].key_columns.clone(),
            require_unique: plan.inputs[1].require_unique,
            output_delimiter: plan.inputs[1].output_delimiter,
            projection: Vec::new(),
            sorted: None,
            frame_bytes: plan.inputs[1].frame_bytes,
        },
    ]);

    // Clone lightweight partition file handles & metadata into Arc so EnginePool workers can run.
    let part_specs: Arc<Vec<([PartitionMeta; 2], PartitionPlan)>> = Arc::new(
        (0..partition_count)
            .map(|p| {
                let m0 = &hash_plan.sides[0].partitions[p];
                let m1 = &hash_plan.sides[1].partitions[p];
                (
                    [
                        PartitionMeta {
                            file: m0.file.clone(),
                            rows: m0.rows,
                            bytes: m0.bytes,
                            key_bytes: m0.key_bytes,
                            max_key_bytes: m0.max_key_bytes,
                            fingerprint: m0.fingerprint,
                        },
                        PartitionMeta {
                            file: m1.file.clone(),
                            rows: m1.rows,
                            bytes: m1.bytes,
                            key_bytes: m1.key_bytes,
                            max_key_bytes: m1.max_key_bytes,
                            fingerprint: m1.fingerprint,
                        },
                    ],
                    hash_plan.partition_plans[p],
                )
            })
            .collect(),
    );

    let ws = Arc::clone(workspace);
    let st = Arc::clone(state);
    let b_arc = Arc::clone(&budget);
    let fb_arc = Arc::clone(&fallback_lock);
    let np_arc = Arc::clone(&next_part);
    let res_arc = Arc::clone(&results);
    let in_arc = Arc::clone(&shared_inputs);
    let specs_arc = Arc::clone(&part_specs);

    run_on_engine_pool(workers, move |_| {
        let mut table = BuildTable::new();
        let mut held_permit: Option<JobBytePermit<'_>> = None;
        loop {
            if st.check().is_err() {
                break;
            }
            let p = np_arc.fetch_add(1, Ordering::AcqRel);
            if p >= partition_count {
                break;
            }
            let (ref pair_metas, part_plan) = specs_arc[p];
            let metas = [&pair_metas[0], &pair_metas[1]];
            let out: JResult<PartitionCreateOut> = (|| {
                if metas[0].rows == 0 && metas[1].rows == 0 {
                    return Ok(PartitionCreateOut {
                        run: None,
                        count: 0,
                        pair_fingerprint: LocatorFingerprint::default(),
                    });
                }
                if metas[0].rows == 0 || metas[1].rows == 0 {
                    table.reset(0);
                    held_permit = None;
                    let present = usize::from(metas[1].rows > 0);
                    let retained = config.mode.retains(present == 0, present == 1);
                    let res = if retained {
                        create_partition_single_side_retained(
                            &in_arc[present],
                            present,
                            metas[present],
                            &ws,
                            config,
                            &st,
                        )?
                    } else {
                        PartitionCreateOut {
                            run: None,
                            count: 0,
                            pair_fingerprint: LocatorFingerprint::default(),
                        }
                    };
                    unlink_partition_pair(metas, &st)?;
                    return Ok(res);
                }

                if !part_plan.use_sort_merge {
                    let est = estimate_build_ram(metas[part_plan.build_side]);
                    if est <= per_partition_cap {
                        let need = est.max(1);
                        let permit_bytes = match held_permit.as_ref() {
                            Some(permit) if permit.bytes >= need => permit.bytes,
                            _ => {
                                table.reset(0);
                                held_permit = None;
                                let permit = b_arc.acquire(need, &st)?;
                                let bytes = permit.bytes;
                                held_permit = Some(permit);
                                bytes
                            }
                        };
                        let effective_cap = permit_bytes.min(per_partition_cap);
                        if let Some(created) = create_partition_hash(
                            &in_arc,
                            metas,
                            part_plan.build_side,
                            &mut table,
                            effective_cap,
                            worker_run_config,
                            &ws,
                            &st,
                        )? {
                            if table.mem_bytes() > permit_bytes {
                                table.reset(0);
                            }
                            unlink_partition_pair(metas, &st)?;
                            return Ok(created);
                        }
                    }
                }
                table.reset(0);
                held_permit = None;
                let mut fb_fatal = fb_arc.lock().unwrap();
                if fb_fatal.is_some() {
                    return Err(Failure::Cancelled);
                }
                let max_key = metas[0].max_key_bytes.max(metas[1].max_key_bytes).max(16);
                let extra_budget = fallback_extra_budget_bytes(config, hash_budget_total, max_key);
                let _fb_permit = if extra_budget > 0 {
                    Some(b_arc.acquire(extra_budget, &st)?)
                } else {
                    None
                };
                match create_partition_sort_merge(&in_arc, metas, &ws, fallback_config, &st) {
                    Ok(created) => {
                        unlink_partition_pair(metas, &st)?;
                        Ok(created)
                    }
                    Err(Failure::Cancelled) => Err(Failure::Cancelled),
                    Err(err) => {
                        // Record the fatal fallback/cleanup failure before releasing `fallback_lock`
                        // so no later worker can start another fallback beyond the single-fallback
                        // disk allowance.
                        *fb_fatal = Some(err);
                        Err(Failure::Cancelled)
                    }
                }
            })();
            if out.is_err() {
                table.reset(0);
                held_permit = None;
            }
            res_arc.lock().unwrap()[p] = Some(out);
        }
        drop(table);
        drop(held_permit);
    })?;

    state.check()?;
    if let Some(err) = fallback_lock.lock().unwrap().take() {
        return Err(err);
    }
    let mut pair_runs: Vec<Run> = Vec::new();
    let part_results = std::mem::take(&mut *results.lock().unwrap());
    for slot in part_results {
        let Some(res) = slot else {
            return Err(Failure::Cancelled);
        };
        let created = res?;
        total_count = add(total_count, created.count)?;
        expected_pair_fp.combine(created.pair_fingerprint);
        if let Some(run) = created.run {
            pair_runs.push(run);
        }
    }
    if total_count != plan.analysis.output_rows {
        return Err(error("Join output differs from the analyzed plan. Analyze again.").into());
    }

    #[cfg(test)]
    crate::join_faults::observe(crate::join_faults::Event::RestoringOrder);

    // Ensure total open streams (pair_runs + retained_empty_sides) <= fan_in before streaming
    // merge directly into materialization. When `fan_in == 2` and both sides retain empty keys
    // alongside non-empty `pair_runs`, convert empty stream(s) into pre-sorted `PairLocator` runs
    // first so `pair_runs.len() + retained_empty_sides.len() <= fan_in` holds strictly.
    let fan_in = config.fan_in(16).min(fd_cap.saturating_sub(4).max(2));
    while pair_runs.len().saturating_add(retained_empty_sides.len()) > fan_in
        && retained_empty_sides.len() >= fan_in
    {
        let side = retained_empty_sides.pop().unwrap();
        let empty = &hash_plan.sides[side].empty;
        let run = create_empty_pair_run(&plan.inputs[side], side, empty, workspace, config, state)?;
        if run.bytes > 0 {
            pair_runs.push(run);
        } else {
            run.path.remove()?;
        }
    }
    let max_pair_runs = fan_in.saturating_sub(retained_empty_sides.len()).max(1);
    if pair_runs.len() > max_pair_runs {
        pair_runs.sort_by_key(|r| r.bytes);
        let merger = Runs::new(workspace, config, state, 16);
        while pair_runs.len() > max_pair_runs {
            let take_count = (pair_runs.len() - max_pair_runs + 1).min(fan_in);
            let group: Vec<Run> = pair_runs.drain(..take_count).collect();
            let merged = merger.merge(group)?;
            pair_runs.push(merged);
            pair_runs.sort_by_key(|r| r.bytes);
        }
    }

    let total_streams = pair_runs.len().saturating_add(retained_empty_sides.len());
    let _merge_fd_guard = if total_streams > 0 {
        Some(
            engine_pool()
                .fd_budget()
                .acquire(total_streams, &state.cancel)
                .ok_or(Failure::Cancelled)?,
        )
    } else {
        None
    };

    let mut streams: Vec<MergeStream> = Vec::with_capacity(total_streams);
    for run in pair_runs {
        let reader = BufReader::with_capacity(READ_CHUNK, run.path.open()?);
        streams.push(MergeStream::PairRun {
            reader,
            file: run.path,
            bytes: run.bytes,
        });
    }
    for side in retained_empty_sides {
        let empty = &hash_plan.sides[side].empty;
        let file = empty
            .file
            .clone()
            .ok_or_else(|| error("Missing private empty-key stream."))?;
        let reader = BufReader::with_capacity(READ_CHUNK, file.open()?);
        streams.push(MergeStream::EmptyRows {
            reader,
            file,
            side,
            bytes: empty.bytes,
            expected_rows: empty.rows,
            expected_fp: empty.fingerprint,
            seen_rows: 0,
            last_ordinal: None,
            observed_fp: LocatorFingerprint::default(),
        });
    }

    let mut heap = BinaryHeap::with_capacity(streams.len());
    for (idx, stream) in streams.iter_mut().enumerate() {
        if let Some((order, pair)) =
            read_next_from_merge_stream(stream, &plan.inputs, config, state)?
        {
            heap.push(MergeHeapEntry {
                order,
                pair,
                stream: idx,
            });
        }
    }

    let mut previous_order: Option<[u8; 16]> = None;
    let materialize_result = plan.materialize_seekable_pairs(
        || {
            let Some(entry) = heap.pop() else {
                return Ok(None);
            };
            if previous_order.is_some_and(|prev| prev >= entry.order) {
                return Err(error("Joined row pairs are not in source order.").into());
            }
            previous_order = Some(entry.order);
            let stream_idx = entry.stream;
            let pair = entry.pair;
            let order = entry.order;
            if let Some((next_order, next_pair)) =
                read_next_from_merge_stream(&mut streams[stream_idx], &plan.inputs, config, state)?
            {
                heap.push(MergeHeapEntry {
                    order: next_order,
                    pair: next_pair,
                    stream: stream_idx,
                });
            }
            Ok(Some((order, pair)))
        },
        expected_pair_fp,
        source_files,
        output,
        state,
    );

    // Unlink all remaining stream files cleanly, subtracting bytes only after removal succeeds.
    let mut cleanup_err: Option<QuarryError> = None;
    for stream in streams {
        let (file, bytes) = match stream {
            MergeStream::PairRun {
                reader,
                file,
                bytes,
            } => {
                drop(reader);
                (file, bytes)
            }
            MergeStream::EmptyRows {
                reader,
                file,
                bytes,
                ..
            } => {
                drop(reader);
                (file, bytes)
            }
        };
        match file.remove() {
            Ok(()) => state.sub_disk(bytes),
            Err(err) => {
                if cleanup_err.is_none() {
                    cleanup_err = Some(err.into());
                }
            }
        }
    }
    drop(_merge_fd_guard);

    match (materialize_result, cleanup_err) {
        (Ok(()), None) => Ok(()),
        (Ok(()), Some(err)) => Err(err.into()),
        (Err(primary), None) => Err(primary),
        (Err(primary), Some(err)) => Err(error(format!(
            "{}. Could not remove private join stream data: {err}",
            primary.description()
        ))
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn job_byte_budget_waiter_observes_cancellation_while_permit_remains_held() {
        let budget = Arc::new(JobByteBudget::new(1024));
        let state = Arc::new(Progress::new());
        let first_permit = budget.acquire(1024, &state).expect("first permit acquired");

        let (wait_tx, wait_rx) = mpsc::channel();
        let (res_tx, res_rx) = mpsc::channel();
        let budget_worker = Arc::clone(&budget);
        let state_worker = Arc::clone(&state);
        let wait_signal = Arc::new(Mutex::new(Some(wait_tx)));
        let waiter = std::thread::spawn(move || {
            let _guard = storage_test::install(storage_test::Hook {
                budget_wait_signal: Some(wait_signal),
                ..storage_test::Hook::default()
            });
            let result = budget_worker
                .acquire(512, &state_worker)
                .map(|permit| permit.bytes);
            let _ = res_tx.send(result);
        });

        wait_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("waiter must enter JobByteBudget::acquire wait");
        state.cancel.store(true, Ordering::Release);

        // Keep `first_permit` held until the waiter reports `Failure::Cancelled`.
        // On timeout, drop `first_permit` first so the waiter can be reaped cleanly.
        let outcome = match res_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(result) => result,
            Err(err) => {
                drop(first_permit);
                let _ = waiter.join();
                panic!("waiter did not observe cancellation while permit remained held: {err}");
            }
        };
        assert!(
            matches!(outcome, Err(Failure::Cancelled)),
            "expected Failure::Cancelled while first permit is still held, got {outcome:?}"
        );
        drop(first_permit);
        waiter.join().unwrap();
    }

    #[test]
    fn retained_tables_reset_incompatible_buffers_before_reserving_growth() {
        let cap = 256 * 1024;
        let mut analysis = AnalysisTable::new();
        let long_key_meta = PartitionMeta {
            rows: 10,
            key_bytes: 200 * 1024,
            ..PartitionMeta::default()
        };
        let empty_meta = PartitionMeta::default();
        analysis.prepare_for_metas([&long_key_meta, &empty_meta], cap);
        assert!(analysis.key_arena.capacity() >= 200 * 1024);
        assert!(analysis.mem_bytes() <= cap);

        // Next partition has tiny keys but many groups (~140 KiB groups+slots): combined with the
        // old 200 KiB key arena would exceed 256 KiB, so `prepare_for_metas` must drop the old
        // arena before reserving.
        let many_groups_meta = PartitionMeta {
            rows: 2_000,
            key_bytes: 16 * 1024,
            ..PartitionMeta::default()
        };
        analysis.prepare_for_metas([&many_groups_meta, &empty_meta], cap);
        assert!(analysis.mem_bytes() <= cap);
        assert!(analysis.groups.capacity() >= 2_000);

        let mut build = BuildTable::new();
        build.prepare_for_meta(&long_key_meta, cap).unwrap();
        assert!(build.key_arena.capacity() >= 200 * 1024);
        assert!(build.mem_bytes() <= cap);

        let many_rows_meta = PartitionMeta {
            rows: 2_500,
            key_bytes: 16 * 1024,
            ..PartitionMeta::default()
        };
        build.prepare_for_meta(&many_rows_meta, cap).unwrap();
        assert!(build.mem_bytes() <= cap);
        assert!(build.rows.capacity() >= 2_500);
    }

    #[test]
    fn inspect_pair_main_failure_returns_main_error_without_scanning_additional() {
        let dir = tempfile::tempdir().unwrap();
        let empty_main = dir.path().join("empty-main.csv");
        let missing_add = dir.path().join("never-opened-add.csv");
        fs::write(&empty_main, b"").unwrap();
        // Even with a single worker, Main failing must skip Additional and return Main's error
        // rather than Additional's error or user cancellation.
        let _guard = storage_test::install(storage_test::Hook {
            force_single_pool_worker: true,
            ..storage_test::Hook::default()
        });
        let state = Arc::new(Progress::new());
        let err = inspect_pair(
            [
                JoinInput {
                    path: empty_main.clone(),
                    delimiter: Some(b','),
                    has_header: true,
                    encoding: Some(Encoding::Utf8),
                    key_columns: vec![0],
                    expected_version: None,
                    require_unique: false,
                },
                JoinInput {
                    path: missing_add,
                    delimiter: Some(b','),
                    has_header: true,
                    encoding: Some(Encoding::Utf8),
                    key_columns: vec![0],
                    expected_version: None,
                    require_unique: false,
                },
            ],
            [None, None],
            JoinConfig::default(),
            &state,
        )
        .err()
        .expect("empty main input must fail");
        assert!(
            matches!(
                err,
                Failure::Error(QuarryError::JoinFile {
                    path: Some(ref p),
                    ..
                }) if *p == empty_main
            ),
            "expected Main JoinFile error, got {err:?}"
        );
        assert!(!state.is_cancelled());
    }
}
