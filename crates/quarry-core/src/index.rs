use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::Encoding;
use crate::encoding::EncodedScanner;
use crate::source_stamp::open_regular_file;

use crate::export::source_matches_stamp;
use crate::{CompletedRecordCount, QuarryError, Session, SourceStamp};

const DEFAULT_INDEX_CHUNK_BYTES: usize = 1024 * 1024;
const DEFAULT_CHECKPOINT_EVERY: u64 = 4096;
const DEFAULT_INDEX_MEMORY_BUDGET: usize = 16 * 1024 * 1024;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralIndex {
    checkpoints: Vec<Checkpoint>,
    checkpoint_every: u64,
    max_checkpoints: usize,
    indexed_rows: u64,
    maximum_width: usize,
    indexed_bytes: u64,
}

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
            max_checkpoints,
            indexed_rows: 0,
            maximum_width: 0,
            indexed_bytes: 0,
        })
    }

    pub(crate) fn record_boundary(&mut self, offset: u64) {
        self.indexed_rows += 1;
        self.indexed_bytes = offset;
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
    index: StructuralIndex,
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
            index,
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
        Ok(completed.index)
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
    pub(crate) fn start(session: &Session, config: IndexConfig) -> Result<Self, QuarryError> {
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
                let result = run_indexer(&file, delimiter, encoding, config, &worker_state)
                    .and_then(|()| {
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
    mut file: &File,
    delimiter: u8,
    encoding: Encoding,
    config: IndexConfig,
    shared: &SharedState,
) -> Result<(), QuarryError> {
    let mut scanner = EncodedScanner::<true>::new(delimiter, encoding)?;
    let mut chunk = vec![0; encoding.chunk_size(config.chunk_bytes)];
    let mut absolute_start = 0;

    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let read = encoding.read_chunk(&mut file, &mut chunk)?;
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
    shared
        .bytes_scanned
        .store(absolute_start, Ordering::Release);
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

    use super::{IndexConfig, IndexJob, SharedState, StructuralIndex, WorkerCompletion};

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
        let session = crate::Session::open(&path, crate::OpenOptions::default()).unwrap();
        let job = IndexJob::start(
            &session,
            IndexConfig {
                chunk_bytes: 1,
                checkpoint_every: 8,
                memory_budget_bytes: 64 * 1024,
            },
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
            let session = crate::Session::open(&path, crate::OpenOptions::default()).unwrap();
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
