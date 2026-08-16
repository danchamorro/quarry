use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use quarry_delimited::RecordScanner;

use crate::QuarryError;

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

#[derive(Debug, Clone)]
pub struct StructuralIndex {
    checkpoints: Vec<Checkpoint>,
    checkpoint_every: u64,
    max_checkpoints: usize,
    indexed_rows: u64,
    indexed_bytes: u64,
}

impl StructuralIndex {
    fn new(config: IndexConfig) -> Result<Self, QuarryError> {
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
            indexed_bytes: 0,
        })
    }

    fn record_boundary(&mut self, offset: u64) {
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

#[derive(Debug, Clone, Copy)]
pub struct IndexProgress {
    pub bytes_scanned: u64,
    pub rows_scanned: u64,
    pub file_size: u64,
    pub elapsed: Duration,
    pub done: bool,
    pub cancelled: bool,
}

struct SharedState {
    index: RwLock<StructuralIndex>,
    bytes_scanned: AtomicU64,
    rows_scanned: AtomicU64,
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
    pub(crate) fn start(
        path: PathBuf,
        file_size: u64,
        delimiter: u8,
        config: IndexConfig,
    ) -> Result<Self, QuarryError> {
        let index = StructuralIndex::new(config)?;
        let file = File::open(path)?;
        let shared = Arc::new(SharedState {
            index: RwLock::new(index),
            bytes_scanned: AtomicU64::new(0),
            rows_scanned: AtomicU64::new(0),
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
                let result = run_indexer(file, delimiter, config, &worker_state);
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
    mut file: File,
    delimiter: u8,
    config: IndexConfig,
    shared: &SharedState,
) -> Result<(), QuarryError> {
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut chunk = vec![0; config.chunk_bytes];
    let mut absolute_start = 0;

    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let read = file.read(&mut chunk)?;
        if read == 0 {
            let mut index = shared.index.write().unwrap();
            scanner.finish(absolute_start, |end| index.record_boundary(end))?;
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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
        let file_size = contents.len() as u64;
        fs::write(&path, contents).unwrap();

        let job = IndexJob::start(
            path.clone(),
            file_size,
            b',',
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

        drop(job);

        assert!(shared.cancelled.load(Ordering::Acquire));
        assert!(shared.done.load(Ordering::Acquire));
        assert!(shared.finished_nanos.load(Ordering::Acquire) > 0);
        fs::remove_file(path).unwrap();
    }
}
