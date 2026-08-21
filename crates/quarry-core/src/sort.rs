use std::cmp::Ordering as CmpOrdering;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::{self, File, OpenOptions};
use std::hash::Hasher;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use quarry_delimited::{RecordScanner, parse_record};

use crate::export::{ExportTarget, source_matches_stamp};
use crate::{
    DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, FilterExportOutcome, QuarryError, Session,
    SourceStamp,
};

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
const DEFAULT_RUN_MEMORY_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MERGE_FAN_IN: usize = 32;
const MERGE_MEMORY_BUDGET_BYTES: usize = 256 * 1024 * 1024;
static NEXT_SORT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub column: usize,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Copy)]
pub struct SortProgress {
    pub bytes_scanned: u64,
    pub total_bytes: u64,
    pub rows_sorted: u64,
    pub runs_created: u64,
    pub bytes_written: u64,
    pub peak_temporary_bytes: u64,
    pub merge_passes: u64,
    pub header_rows: u64,
    pub elapsed: Duration,
    pub cancellation_latency: Option<Duration>,
    pub done: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortSummary {
    pub destination: PathBuf,
    pub rows_sorted: u64,
    pub bytes_written: u64,
    pub runs_created: u64,
    pub peak_temporary_bytes: u64,
    pub merge_passes: u64,
    pub header_rows: u64,
    pub elapsed: Duration,
    pub record_multiset_verified: bool,
    pub stable_ties_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SortOutcome {
    Complete(SortSummary),
    Cancelled,
}

pub const fn estimate_sort_temporary_bytes(
    effective_bytes_upper_bound: u64,
    data_rows: u64,
) -> u64 {
    effective_bytes_upper_bound
        .saturating_mul(4)
        .saturating_add(data_rows.saturating_mul(48))
}

#[derive(Clone, Copy)]
struct SortConfig {
    chunk_bytes: usize,
    max_record_bytes: usize,
    run_memory_bytes: usize,
    merge_fan_in: usize,
}

const DEFAULT_SORT_CONFIG: SortConfig = SortConfig {
    chunk_bytes: DEFAULT_READ_CHUNK,
    max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
    run_memory_bytes: DEFAULT_RUN_MEMORY_BYTES,
    merge_fan_in: DEFAULT_MERGE_FAN_IN,
};

struct SharedState {
    bytes_scanned: AtomicU64,
    total_bytes: u64,
    rows_sorted: AtomicU64,
    runs_created: AtomicU64,
    bytes_written: AtomicU64,
    temporary_bytes: AtomicU64,
    peak_temporary_bytes: AtomicU64,
    merge_passes: AtomicU64,
    header_rows: AtomicU64,
    started: Instant,
    elapsed_nanos: AtomicU64,
    cancel_requested_at: Mutex<Option<Instant>>,
    cancellation_nanos: AtomicU64,
    done: AtomicBool,
    cancelled: AtomicBool,
    cancel_requested: AtomicBool,
    error: Mutex<Option<String>>,
}

impl SharedState {
    fn new(total_bytes: u64) -> Self {
        Self {
            bytes_scanned: AtomicU64::new(0),
            total_bytes,
            rows_sorted: AtomicU64::new(0),
            runs_created: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            temporary_bytes: AtomicU64::new(0),
            peak_temporary_bytes: AtomicU64::new(0),
            merge_passes: AtomicU64::new(0),
            header_rows: AtomicU64::new(0),
            started: Instant::now(),
            elapsed_nanos: AtomicU64::new(0),
            cancel_requested_at: Mutex::new(None),
            cancellation_nanos: AtomicU64::new(u64::MAX),
            done: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            cancel_requested: AtomicBool::new(false),
            error: Mutex::new(None),
        }
    }

    fn add_temporary_bytes(&self, bytes: u64) {
        let current = self
            .temporary_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_add(bytes))
            })
            .expect("temporary byte update cannot fail")
            .saturating_add(bytes);
        self.peak_temporary_bytes
            .fetch_max(current, Ordering::AcqRel);
    }

    fn remove_temporary_bytes(&self, bytes: u64) {
        let _ = self
            .temporary_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(bytes))
            });
    }

    fn request_cancel(&self) {
        let mut requested_at = self.cancel_requested_at.lock().unwrap();
        if self.done.load(Ordering::Acquire) {
            return;
        }
        if !self.cancel_requested.swap(true, Ordering::AcqRel) {
            *requested_at = Some(Instant::now());
        }
    }

    fn elapsed(&self) -> Duration {
        if self.done.load(Ordering::Acquire) {
            Duration::from_nanos(self.elapsed_nanos.load(Ordering::Acquire))
        } else {
            self.started.elapsed()
        }
    }

    fn cancellation_latency(&self) -> Option<Duration> {
        let completed = self.cancellation_nanos.load(Ordering::Acquire);
        if completed != u64::MAX {
            return Some(Duration::from_nanos(completed));
        }
        self.cancel_requested_at
            .lock()
            .unwrap()
            .as_ref()
            .map(Instant::elapsed)
    }

    fn finish(&self) -> Duration {
        let elapsed = self.started.elapsed();
        self.elapsed_nanos
            .store(duration_nanos(elapsed), Ordering::Release);
        if let Some(requested_at) = *self.cancel_requested_at.lock().unwrap() {
            self.cancellation_nanos
                .store(duration_nanos(requested_at.elapsed()), Ordering::Release);
        }
        self.done.store(true, Ordering::Release);
        elapsed
    }
}

fn duration_nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128 - 1) as u64
}

struct WorkerCompletion<'a> {
    shared: &'a SharedState,
    finished: bool,
}

impl<'a> WorkerCompletion<'a> {
    fn new(shared: &'a SharedState) -> Self {
        Self {
            shared,
            finished: false,
        }
    }

    fn finish(&mut self) -> Duration {
        let elapsed = self.shared.finish();
        self.finished = true;
        elapsed
    }
}

impl Drop for WorkerCompletion<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.shared.finish();
        }
    }
}

pub struct SortJob {
    shared: Arc<SharedState>,
    handle: Option<JoinHandle<Result<SortOutcome, QuarryError>>>,
}

impl SortJob {
    #[allow(clippy::too_many_arguments)]
    fn start(
        source_path: PathBuf,
        file_size: u64,
        delimiter: u8,
        has_header: bool,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        spec: SortSpec,
        destination: PathBuf,
        source_stamp: SourceStamp,
        config: SortConfig,
    ) -> Result<Self, QuarryError> {
        validate_config(config)?;
        if !has_header && !header_renames.is_empty() {
            return Err(QuarryError::InvalidOption(
                "header renames require a header row",
            ));
        }
        let data_start = u64::from(has_header);
        if cell_edits
            .first_key_value()
            .is_some_and(|((row, _), _)| *row < data_start)
        {
            return Err(QuarryError::InvalidOption(
                "cell edits must target data rows",
            ));
        }
        if header_renames
            .values()
            .chain(cell_edits.values())
            .any(|value| value.len() > config.max_record_bytes)
        {
            return Err(QuarryError::RecordTooLarge {
                limit: config.max_record_bytes,
            });
        }

        let mut source = File::open(&source_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
        if !source_matches_stamp(&source, &source_path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        let bom_present = source_has_bom(&mut source)?;
        let output = ExportTarget::new_private_guarded(
            &source_path,
            destination.clone(),
            &source,
            source_stamp.clone(),
        )?;

        let shared = Arc::new(SharedState::new(file_size));
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-sort".into())
            .spawn(move || {
                let mut completion = WorkerCompletion::new(&worker_state);
                let mut result = run_sort(
                    &mut source,
                    &source_path,
                    &source_stamp,
                    delimiter,
                    has_header,
                    &header_renames,
                    &cell_edits,
                    spec,
                    destination,
                    output,
                    bom_present,
                    config,
                    &worker_state,
                );
                match &result {
                    Ok(SortOutcome::Cancelled) => {
                        worker_state.cancelled.store(true, Ordering::Release);
                    }
                    Err(error) => {
                        *worker_state.error.lock().unwrap() = Some(error.to_string());
                    }
                    Ok(SortOutcome::Complete(_)) => {}
                }
                let elapsed = completion.finish();
                if let Ok(SortOutcome::Complete(summary)) = &mut result {
                    summary.elapsed = elapsed;
                }
                result
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> SortProgress {
        SortProgress {
            bytes_scanned: self.shared.bytes_scanned.load(Ordering::Acquire),
            total_bytes: self.shared.total_bytes,
            rows_sorted: self.shared.rows_sorted.load(Ordering::Acquire),
            runs_created: self.shared.runs_created.load(Ordering::Acquire),
            bytes_written: self.shared.bytes_written.load(Ordering::Acquire),
            peak_temporary_bytes: self.shared.peak_temporary_bytes.load(Ordering::Acquire),
            merge_passes: self.shared.merge_passes.load(Ordering::Acquire),
            header_rows: self.shared.header_rows.load(Ordering::Acquire),
            elapsed: self.shared.elapsed(),
            cancellation_latency: self.shared.cancellation_latency(),
            done: self.shared.done.load(Ordering::Acquire),
            cancelled: self.shared.cancelled.load(Ordering::Acquire),
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.shared.request_cancel();
    }

    pub fn wait(mut self) -> Result<SortOutcome, QuarryError> {
        self.handle
            .take()
            .expect("sort handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl Drop for SortJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.shared.request_cancel();
            let _ = handle.join();
        }
    }
}

impl Session {
    pub fn start_create_sorted_working_copy(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        spec: SortSpec,
        destination: impl AsRef<Path>,
    ) -> Result<SortJob, QuarryError> {
        SortJob::start(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            header_renames,
            cell_edits,
            spec,
            destination.as_ref().to_path_buf(),
            self.source_stamp.clone(),
            DEFAULT_SORT_CONFIG,
        )
    }
}

fn validate_config(config: SortConfig) -> Result<(), QuarryError> {
    if config.chunk_bytes == 0 || config.run_memory_bytes == 0 || config.max_record_bytes == 0 {
        return Err(QuarryError::InvalidOption(
            "sort chunk, run memory, and record limit must be non-zero",
        ));
    }
    if config.merge_fan_in < 2 {
        return Err(QuarryError::InvalidOption(
            "sort merge fan-in must be at least two",
        ));
    }
    if config.max_record_bytes.saturating_mul(4) > MERGE_MEMORY_BUDGET_BYTES {
        return Err(QuarryError::InvalidOption(
            "sort record limit exceeds the merge memory budget",
        ));
    }
    Ok(())
}

fn effective_merge_fan_in(config: SortConfig) -> usize {
    let total_slots = MERGE_MEMORY_BUDGET_BYTES / config.max_record_bytes;
    let key_slots = total_slots.saturating_sub(2);
    config.merge_fan_in.min(key_slots.max(2))
}

fn source_has_bom(source: &mut File) -> Result<bool, QuarryError> {
    let mut prefix = [0_u8; 3];
    let mut read = 0;
    while read < prefix.len() {
        let count = source.read(&mut prefix[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    source.seek(SeekFrom::Start(0))?;
    Ok(read == prefix.len() && prefix == UTF8_BOM)
}

struct RunWorkspace {
    path: PathBuf,
    next_run: AtomicU64,
    active: bool,
}

impl RunWorkspace {
    fn create(destination: &Path) -> Result<Self, QuarryError> {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        for _ in 0..100 {
            let id = NEXT_SORT_ID.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".quarry-sort-{}-{id}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        next_run: AtomicU64::new(0),
                        active: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a temporary sort directory",
        )
        .into())
    }

    fn create_run(&self) -> Result<(PathBuf, BufWriter<File>), QuarryError> {
        let id = self.next_run.fetch_add(1, Ordering::Relaxed);
        let path = self.path.join(format!("run-{id}.bin"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path)?;
        Ok((path, BufWriter::new(file)))
    }

    fn cleanup(mut self) -> Result<(), QuarryError> {
        match fs::remove_dir_all(&self.path) {
            Ok(()) => {
                self.active = false;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.active = false;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for RunWorkspace {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RunEntry {
    key: Vec<u8>,
    record: Vec<u8>,
    ordinal: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct RunHead {
    key: Vec<u8>,
    record_len: usize,
    ordinal: u64,
}

fn compare_entries(left: &RunEntry, right: &RunEntry, direction: SortDirection) -> CmpOrdering {
    let key_order = match direction {
        SortDirection::Ascending => left.key.cmp(&right.key),
        SortDirection::Descending => right.key.cmp(&left.key),
    };
    key_order.then_with(|| left.ordinal.cmp(&right.ordinal))
}

#[derive(Debug, PartialEq, Eq)]
struct HeapEntry {
    head: RunHead,
    run_index: usize,
    direction: SortDirection,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        if self.direction != other.direction {
            return self.direction.cmp(&other.direction);
        }
        compare_heads(&self.head, &other.head, self.direction)
            .reverse()
            .then_with(|| other.run_index.cmp(&self.run_index))
    }
}

fn compare_heads(left: &RunHead, right: &RunHead, direction: SortDirection) -> CmpOrdering {
    let key_order = match direction {
        SortDirection::Ascending => left.key.cmp(&right.key),
        SortDirection::Descending => right.key.cmp(&left.key),
    };
    key_order.then_with(|| left.ordinal.cmp(&right.ordinal))
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

const RECORD_HASH_DOMAIN_A: u64 = 0x7175_6172_7279_2d61;
const RECORD_HASH_DOMAIN_B: u64 = 0x7175_6172_7279_2d62;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RecordMultisetFingerprint {
    count: u64,
    xor_a: u64,
    sum_a: u64,
    xor_b: u64,
    sum_b: u64,
}

impl RecordMultisetFingerprint {
    fn observe(&mut self, record: &[u8]) {
        let hash_a = hash_bytes(record, RECORD_HASH_DOMAIN_A);
        let hash_b = hash_bytes(record, RECORD_HASH_DOMAIN_B);
        self.count = self.count.saturating_add(1);
        self.xor_a ^= hash_a;
        self.sum_a = self.sum_a.wrapping_add(hash_a);
        self.xor_b ^= hash_b;
        self.sum_b = self.sum_b.wrapping_add(hash_b);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SortVerification {
    record_multiset_verified: bool,
    stable_ties_verified: bool,
}

struct OutputVerifier {
    expected_records: RecordMultisetFingerprint,
    observed_records: RecordMultisetFingerprint,
    adjacencies_checked: u64,
}

impl OutputVerifier {
    fn new(expected_records: RecordMultisetFingerprint) -> Self {
        Self {
            expected_records,
            observed_records: RecordMultisetFingerprint::default(),
            adjacencies_checked: 0,
        }
    }

    fn verify_adjacent(
        &mut self,
        previous_key: &[u8],
        previous_ordinal: u64,
        current_key: &[u8],
        current_ordinal: u64,
    ) -> Result<(), QuarryError> {
        if previous_key == current_key && current_ordinal <= previous_ordinal {
            return Err(invalid_sort_output(
                "sorted output changed equal-key row order",
            ));
        }
        self.adjacencies_checked = self.adjacencies_checked.saturating_add(1);
        Ok(())
    }

    fn observe(&mut self, record: &[u8], inserted_ending: &[u8]) -> Result<(), QuarryError> {
        if !inserted_ending.is_empty()
            && (!record_ending(record).is_empty() || !matches!(inserted_ending, b"\n" | b"\r\n"))
        {
            return Err(invalid_sort_output(
                "sorted output inserted an invalid record ending",
            ));
        }
        self.observed_records.observe(record);
        Ok(())
    }

    fn finish(self) -> Result<SortVerification, QuarryError> {
        if self.observed_records != self.expected_records {
            return Err(invalid_sort_output(
                "sorted output did not preserve the effective record multiset",
            ));
        }
        if self.adjacencies_checked != self.observed_records.count.saturating_sub(1) {
            return Err(invalid_sort_output(
                "sorted output did not verify every adjacent row",
            ));
        }
        Ok(SortVerification {
            record_multiset_verified: true,
            stable_ties_verified: true,
        })
    }
}

fn hash_bytes(bytes: &[u8], domain: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write_u64(domain);
    hasher.write_u64(bytes.len() as u64);
    hasher.write(bytes);
    hasher.finish()
}

fn invalid_sort_output(message: &'static str) -> QuarryError {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

struct ScanSummary {
    header: Option<Vec<u8>>,
    preferred_ending: Vec<u8>,
    runs: Vec<PathBuf>,
    rows: u64,
    records: RecordMultisetFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuilderStep {
    Complete,
    Cancelled,
}

struct InitialRunBuilder<'a> {
    delimiter: u8,
    has_header: bool,
    header_renames: &'a BTreeMap<usize, Vec<u8>>,
    cell_edits: &'a BTreeMap<(u64, usize), Vec<u8>>,
    spec: SortSpec,
    bom_present: bool,
    config: SortConfig,
    workspace: &'a RunWorkspace,
    shared: &'a SharedState,
    header: Option<Vec<u8>>,
    preferred_ending: Vec<u8>,
    ending_seen: bool,
    entries: Vec<RunEntry>,
    entry_bytes: usize,
    runs: Vec<PathBuf>,
    rows: u64,
    records: RecordMultisetFingerprint,
}

impl<'a> InitialRunBuilder<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        delimiter: u8,
        has_header: bool,
        header_renames: &'a BTreeMap<usize, Vec<u8>>,
        cell_edits: &'a BTreeMap<(u64, usize), Vec<u8>>,
        spec: SortSpec,
        bom_present: bool,
        config: SortConfig,
        workspace: &'a RunWorkspace,
        shared: &'a SharedState,
    ) -> Self {
        Self {
            delimiter,
            has_header,
            header_renames,
            cell_edits,
            spec,
            bom_present,
            config,
            workspace,
            shared,
            header: None,
            preferred_ending: b"\n".to_vec(),
            ending_seen: false,
            entries: Vec::new(),
            entry_bytes: 0,
            runs: Vec::new(),
            rows: 0,
            records: RecordMultisetFingerprint::default(),
        }
    }

    fn process(&mut self, record: &[u8], physical_row: u64) -> Result<BuilderStep, QuarryError> {
        if record.len() > self.config.max_record_bytes {
            return Err(QuarryError::RecordTooLarge {
                limit: self.config.max_record_bytes,
            });
        }
        let body = if physical_row == 0 && self.bom_present {
            record.strip_prefix(UTF8_BOM).unwrap_or(record)
        } else {
            record
        };
        let ending = record_ending(body);
        if !ending.is_empty() && !self.ending_seen {
            self.preferred_ending = ending.to_vec();
            self.ending_seen = true;
        }
        let fields = parse_record(body, self.delimiter)?;

        if self.has_header && physical_row == 0 {
            if self
                .header_renames
                .last_key_value()
                .is_some_and(|(column, _)| *column >= fields.len())
            {
                return Err(QuarryError::InvalidOption(
                    "header rename column is out of range",
                ));
            }
            let header = if self.header_renames.is_empty() {
                body.to_vec()
            } else {
                let mut values: Vec<Vec<u8>> = fields.iter().map(|field| field.to_vec()).collect();
                for (column, value) in self.header_renames {
                    values[*column] = value.clone();
                }
                serialize_fields(
                    &values,
                    self.delimiter,
                    ending,
                    self.config.max_record_bytes,
                )?
            };
            if header
                .len()
                .saturating_add(usize::from(self.bom_present) * UTF8_BOM.len())
                > self.config.max_record_bytes
            {
                return Err(QuarryError::RecordTooLarge {
                    limit: self.config.max_record_bytes,
                });
            }
            self.header = Some(header);
            self.shared.header_rows.store(1, Ordering::Release);
            return Ok(BuilderStep::Complete);
        }

        let row_edits: Vec<(usize, &[u8])> = self
            .cell_edits
            .range((physical_row, 0)..=(physical_row, usize::MAX))
            .map(|((_, column), value)| (*column, value.as_slice()))
            .collect();
        if row_edits.iter().any(|(column, _)| *column >= fields.len()) {
            return Err(QuarryError::InvalidOption(
                "cell edit column is out of range",
            ));
        }
        let key = row_edits
            .iter()
            .find(|(column, _)| *column == self.spec.column)
            .map(|(_, value)| (*value).to_vec())
            .or_else(|| fields.get(self.spec.column).map(|field| field.to_vec()))
            .unwrap_or_default();
        let effective_record =
            if row_edits.is_empty() && !(physical_row > 0 && body.starts_with(UTF8_BOM)) {
                body.to_vec()
            } else {
                let mut values: Vec<Vec<u8>> = fields.iter().map(|field| field.to_vec()).collect();
                for (column, value) in row_edits {
                    values[column] = value.to_vec();
                }
                serialize_fields(
                    &values,
                    self.delimiter,
                    ending,
                    self.config.max_record_bytes,
                )?
            };
        if physical_row == 0
            && self.bom_present
            && effective_record.len().saturating_add(UTF8_BOM.len()) > self.config.max_record_bytes
        {
            return Err(QuarryError::RecordTooLarge {
                limit: self.config.max_record_bytes,
            });
        }

        let entry_size = key
            .len()
            .saturating_add(effective_record.len())
            .saturating_add(24);
        if !self.entries.is_empty()
            && self.entry_bytes.saturating_add(entry_size) > self.config.run_memory_bytes
            && self.flush()? == BuilderStep::Cancelled
        {
            return Ok(BuilderStep::Cancelled);
        }
        self.records.observe(&effective_record);
        self.entries.push(RunEntry {
            key,
            record: effective_record,
            ordinal: self.rows,
        });
        self.entry_bytes = self.entry_bytes.saturating_add(entry_size);
        self.rows = self.rows.saturating_add(1);
        self.shared.rows_sorted.store(self.rows, Ordering::Release);
        Ok(BuilderStep::Complete)
    }

    fn flush(&mut self) -> Result<BuilderStep, QuarryError> {
        if self.entries.is_empty() {
            return Ok(BuilderStep::Complete);
        }
        self.entries
            .sort_by(|left, right| compare_entries(left, right, self.spec.direction));
        let (path, mut writer) = self.workspace.create_run()?;
        for entry in self.entries.drain(..) {
            if self.shared.cancel_requested.load(Ordering::Acquire) {
                return Ok(BuilderStep::Cancelled);
            }
            let bytes = write_entry(&mut writer, &entry)?;
            self.shared.add_temporary_bytes(bytes);
        }
        writer.flush()?;
        self.runs.push(path);
        self.entry_bytes = 0;
        self.shared.runs_created.fetch_add(1, Ordering::AcqRel);
        Ok(BuilderStep::Complete)
    }

    fn finish(mut self, physical_rows: u64) -> Result<ScanOutcome, QuarryError> {
        if self.has_header && self.header.is_none() {
            return Err(QuarryError::InvalidOption(
                "source does not contain a header row",
            ));
        }
        if self
            .cell_edits
            .last_key_value()
            .is_some_and(|((row, _), _)| *row >= physical_rows)
        {
            return Err(QuarryError::InvalidOption("cell edit row is out of range"));
        }
        if self.flush()? == BuilderStep::Cancelled {
            return Ok(ScanOutcome::Cancelled);
        }
        Ok(ScanOutcome::Complete(ScanSummary {
            header: self.header,
            preferred_ending: self.preferred_ending,
            runs: self.runs,
            rows: self.rows,
            records: self.records,
        }))
    }
}

enum ScanOutcome {
    Complete(ScanSummary),
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
fn create_initial_runs(
    source: &mut File,
    delimiter: u8,
    has_header: bool,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    spec: SortSpec,
    bom_present: bool,
    config: SortConfig,
    workspace: &RunWorkspace,
    shared: &SharedState,
) -> Result<ScanOutcome, QuarryError> {
    source.seek(SeekFrom::Start(0))?;
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut chunk = vec![0_u8; config.chunk_bytes];
    let mut record = Vec::new();
    let mut absolute_start = 0_u64;
    let mut physical_row = 0_u64;
    let mut builder = InitialRunBuilder::new(
        delimiter,
        has_header,
        header_renames,
        cell_edits,
        spec,
        bom_present,
        config,
        workspace,
        shared,
    );

    loop {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(ScanOutcome::Cancelled);
        }
        let read = source.read(&mut chunk)?;
        if read == 0 {
            let mut deferred_error = None;
            let mut cancelled = false;
            let finish_result = scanner.finish(absolute_start, |_| {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    cancelled = true;
                } else {
                    match builder.process(&record, physical_row) {
                        Ok(BuilderStep::Complete) => {}
                        Ok(BuilderStep::Cancelled) => cancelled = true,
                        Err(error) => deferred_error = Some(error),
                    }
                }
                record.clear();
                physical_row = physical_row.saturating_add(1);
            });
            shared
                .bytes_scanned
                .store(absolute_start, Ordering::Release);
            if cancelled {
                return Ok(ScanOutcome::Cancelled);
            }
            if let Some(error) = deferred_error {
                return Err(error);
            }
            finish_result?;
            return builder.finish(physical_row);
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
                    record.extend_from_slice(&chunk[segment_start..local_end]);
                    if record.len() > config.max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: config.max_record_bytes,
                        });
                    } else {
                        match builder.process(&record, physical_row) {
                            Ok(BuilderStep::Complete) => {}
                            Ok(BuilderStep::Cancelled) => cancelled = true,
                            Err(error) => deferred_error = Some(error),
                        }
                    }
                }
            }
            record.clear();
            physical_row = physical_row.saturating_add(1);
            segment_start = local_end;
        });
        absolute_start = absolute_start.saturating_add(read as u64);
        shared
            .bytes_scanned
            .store(absolute_start, Ordering::Release);
        if cancelled || shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(ScanOutcome::Cancelled);
        }
        if let Some(error) = deferred_error {
            return Err(error);
        }
        scan_result?;
        record.extend_from_slice(&chunk[segment_start..read]);
        if record.len() > config.max_record_bytes {
            return Err(QuarryError::RecordTooLarge {
                limit: config.max_record_bytes,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_sort(
    source: &mut File,
    source_path: &Path,
    source_stamp: &SourceStamp,
    delimiter: u8,
    has_header: bool,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    spec: SortSpec,
    destination: PathBuf,
    mut output: ExportTarget,
    bom_present: bool,
    config: SortConfig,
    shared: &SharedState,
) -> Result<SortOutcome, QuarryError> {
    let workspace = RunWorkspace::create(&destination)?;
    let built = run_sort_inner(
        source,
        source_path,
        source_stamp,
        delimiter,
        has_header,
        header_renames,
        cell_edits,
        spec,
        &mut output,
        bom_present,
        config,
        &workspace,
        shared,
    );
    let cleanup = workspace.cleanup();
    let built = built?;
    cleanup?;
    let Some((rows, bytes_written, verification)) = built else {
        return Ok(SortOutcome::Cancelled);
    };
    match output.publish(rows, bytes_written, &shared.cancel_requested)? {
        FilterExportOutcome::Complete(summary) => Ok(SortOutcome::Complete(SortSummary {
            destination: summary.destination,
            rows_sorted: rows,
            bytes_written: summary.bytes_written,
            runs_created: shared.runs_created.load(Ordering::Acquire),
            peak_temporary_bytes: shared.peak_temporary_bytes.load(Ordering::Acquire),
            merge_passes: shared.merge_passes.load(Ordering::Acquire),
            header_rows: shared.header_rows.load(Ordering::Acquire),
            elapsed: Duration::ZERO,
            record_multiset_verified: verification.record_multiset_verified,
            stable_ties_verified: verification.stable_ties_verified,
        })),
        FilterExportOutcome::Cancelled => Ok(SortOutcome::Cancelled),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_sort_inner(
    source: &mut File,
    source_path: &Path,
    source_stamp: &SourceStamp,
    delimiter: u8,
    has_header: bool,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    spec: SortSpec,
    output: &mut ExportTarget,
    bom_present: bool,
    config: SortConfig,
    workspace: &RunWorkspace,
    shared: &SharedState,
) -> Result<Option<(u64, u64, SortVerification)>, QuarryError> {
    let ScanOutcome::Complete(mut scan) = create_initial_runs(
        source,
        delimiter,
        has_header,
        header_renames,
        cell_edits,
        spec,
        bom_present,
        config,
        workspace,
        shared,
    )?
    else {
        return Ok(None);
    };
    if !source_matches_stamp(source, source_path, source_stamp)? {
        return Err(QuarryError::SourceChanged);
    }
    if shared.cancel_requested.load(Ordering::Acquire) {
        return Ok(None);
    }

    let merge_fan_in = effective_merge_fan_in(config);
    while scan.runs.len() > merge_fan_in {
        let mut next = Vec::with_capacity(scan.runs.len().div_ceil(merge_fan_in));
        for group in scan.runs.chunks(merge_fan_in) {
            if shared.cancel_requested.load(Ordering::Acquire) {
                return Ok(None);
            }
            let (path, writer) = workspace.create_run()?;
            if !merge_runs_to_run(
                group,
                writer,
                spec.direction,
                config.max_record_bytes,
                shared,
            )? {
                return Ok(None);
            }
            for old in group {
                let bytes = fs::metadata(old)?.len();
                fs::remove_file(old)?;
                shared.remove_temporary_bytes(bytes);
            }
            next.push(path);
            shared.runs_created.fetch_add(1, Ordering::AcqRel);
        }
        scan.runs = next;
        shared.merge_passes.fetch_add(1, Ordering::AcqRel);
    }

    let mut bytes_written = 0_u64;
    if bom_present {
        write_output(output, UTF8_BOM, shared)?;
        bytes_written = UTF8_BOM.len() as u64;
    }
    if let Some(header) = &scan.header {
        write_output(output, header, shared)?;
        bytes_written = bytes_written.saturating_add(header.len() as u64);
    }
    shared.bytes_written.store(bytes_written, Ordering::Release);

    let Some((rows_written, data_bytes, verification)) = merge_runs_to_output(
        &scan.runs,
        output,
        spec.direction,
        &scan.preferred_ending,
        scan.records,
        config.max_record_bytes,
        bytes_written,
        shared,
    )?
    else {
        return Ok(None);
    };
    if !scan.runs.is_empty() {
        shared.merge_passes.fetch_add(1, Ordering::AcqRel);
    }
    if rows_written != scan.rows {
        return Err(
            io::Error::new(io::ErrorKind::InvalidData, "sort run row count changed").into(),
        );
    }
    bytes_written = bytes_written.saturating_add(data_bytes);
    shared.bytes_written.store(bytes_written, Ordering::Release);
    Ok(Some((scan.rows, bytes_written, verification)))
}

fn write_entry(writer: &mut BufWriter<File>, entry: &RunEntry) -> Result<u64, QuarryError> {
    write_entry_parts(writer, &entry.key, &entry.record, entry.ordinal)
}

fn write_entry_parts(
    writer: &mut BufWriter<File>,
    key: &[u8],
    record: &[u8],
    ordinal: u64,
) -> Result<u64, QuarryError> {
    writer.write_all(&(key.len() as u64).to_le_bytes())?;
    writer.write_all(&(record.len() as u64).to_le_bytes())?;
    writer.write_all(&ordinal.to_le_bytes())?;
    writer.write_all(key)?;
    writer.write_all(record)?;
    Ok(24_u64
        .saturating_add(key.len() as u64)
        .saturating_add(record.len() as u64))
}

fn read_head(
    reader: &mut BufReader<File>,
    max_record_bytes: usize,
) -> Result<Option<RunHead>, QuarryError> {
    let mut header = [0_u8; 24];
    let read = reader.read(&mut header[..1])?;
    if read == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..])?;
    let key_len = u64::from_le_bytes(header[0..8].try_into().unwrap());
    let record_len = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let ordinal = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let key_len = usize::try_from(key_len)
        .ok()
        .filter(|length| *length <= max_record_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid sort key length"))?;
    let record_len = usize::try_from(record_len)
        .ok()
        .filter(|length| *length <= max_record_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid sort record length"))?;
    let mut key = vec![0_u8; key_len];
    reader.read_exact(&mut key)?;
    Ok(Some(RunHead {
        key,
        record_len,
        ordinal,
    }))
}

fn read_record(reader: &mut BufReader<File>, length: usize) -> Result<Vec<u8>, QuarryError> {
    let mut record = vec![0_u8; length];
    reader.read_exact(&mut record)?;
    Ok(record)
}

fn open_run_readers(paths: &[PathBuf]) -> Result<Vec<BufReader<File>>, QuarryError> {
    paths
        .iter()
        .map(|path| File::open(path).map(BufReader::new).map_err(Into::into))
        .collect()
}

fn seed_heap(
    readers: &mut [BufReader<File>],
    direction: SortDirection,
    max_record_bytes: usize,
    shared: &SharedState,
) -> Result<Option<BinaryHeap<HeapEntry>>, QuarryError> {
    let mut heap = BinaryHeap::with_capacity(readers.len());
    for (run_index, reader) in readers.iter_mut().enumerate() {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        if let Some(head) = read_head(reader, max_record_bytes)? {
            if shared.cancel_requested.load(Ordering::Acquire) {
                return Ok(None);
            }
            heap.push(HeapEntry {
                head,
                run_index,
                direction,
            });
        }
    }
    Ok(Some(heap))
}

fn merge_runs_to_run(
    paths: &[PathBuf],
    mut writer: BufWriter<File>,
    direction: SortDirection,
    max_record_bytes: usize,
    shared: &SharedState,
) -> Result<bool, QuarryError> {
    let mut readers = open_run_readers(paths)?;
    let Some(mut heap) = seed_heap(&mut readers, direction, max_record_bytes, shared)? else {
        return Ok(false);
    };
    while let Some(item) = heap.pop() {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(false);
        }
        let record = read_record(&mut readers[item.run_index], item.head.record_len)?;
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(false);
        }
        let bytes = write_entry_parts(&mut writer, &item.head.key, &record, item.head.ordinal)?;
        shared.add_temporary_bytes(bytes);
        let run_index = item.run_index;
        drop(record);
        drop(item);
        if let Some(head) = read_head(&mut readers[run_index], max_record_bytes)? {
            heap.push(HeapEntry {
                head,
                run_index,
                direction,
            });
        }
    }
    writer.flush()?;
    Ok(true)
}

struct PendingOutputEntry {
    key: Vec<u8>,
    record: Vec<u8>,
    ordinal: u64,
}

#[allow(clippy::too_many_arguments)]
fn merge_runs_to_output(
    paths: &[PathBuf],
    output: &mut ExportTarget,
    direction: SortDirection,
    preferred_ending: &[u8],
    expected_records: RecordMultisetFingerprint,
    max_record_bytes: usize,
    prior_bytes_written: u64,
    shared: &SharedState,
) -> Result<Option<(u64, u64, SortVerification)>, QuarryError> {
    let mut readers = open_run_readers(paths)?;
    let Some(mut heap) = seed_heap(&mut readers, direction, max_record_bytes, shared)? else {
        return Ok(None);
    };
    let mut verifier = OutputVerifier::new(expected_records);
    let mut pending: Option<PendingOutputEntry> = None;
    let mut rows = 0_u64;
    let mut bytes_written = 0_u64;
    while let Some(item) = heap.pop() {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        if let Some(previous) = pending.take() {
            verifier.verify_adjacent(
                &previous.key,
                previous.ordinal,
                &item.head.key,
                item.head.ordinal,
            )?;
            let inserted_ending = if previous.record.ends_with(b"\n") {
                b"".as_slice()
            } else {
                preferred_ending
            };
            if previous.record.len().saturating_add(inserted_ending.len()) > max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: max_record_bytes,
                });
            }
            let written = write_verified_output_entry(
                output,
                previous,
                inserted_ending,
                &mut verifier,
                shared,
            )?;
            bytes_written = bytes_written.saturating_add(written);
        }
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        let record = read_record(&mut readers[item.run_index], item.head.record_len)?;
        let run_index = item.run_index;
        pending = Some(PendingOutputEntry {
            key: item.head.key,
            record,
            ordinal: item.head.ordinal,
        });
        rows = rows.saturating_add(1);
        if let Some(head) = read_head(&mut readers[run_index], max_record_bytes)? {
            heap.push(HeapEntry {
                head,
                run_index,
                direction,
            });
        }
        shared.bytes_written.store(
            prior_bytes_written.saturating_add(bytes_written),
            Ordering::Release,
        );
    }
    if let Some(last) = pending {
        let written = write_verified_output_entry(output, last, b"", &mut verifier, shared)?;
        bytes_written = bytes_written.saturating_add(written);
    }
    let verification = verifier.finish()?;
    Ok(Some((rows, bytes_written, verification)))
}

fn write_verified_output_entry(
    output: &mut ExportTarget,
    entry: PendingOutputEntry,
    inserted_ending: &[u8],
    verifier: &mut OutputVerifier,
    shared: &SharedState,
) -> Result<u64, QuarryError> {
    let PendingOutputEntry { record, .. } = entry;
    write_output(output, &record, shared)?;
    if !inserted_ending.is_empty() {
        write_output(output, inserted_ending, shared)?;
    }
    verifier.observe(&record, inserted_ending)?;
    Ok((record.len() as u64).saturating_add(inserted_ending.len() as u64))
}

fn write_output(
    output: &mut ExportTarget,
    bytes: &[u8],
    shared: &SharedState,
) -> Result<(), QuarryError> {
    output.write_all(bytes)?;
    shared.add_temporary_bytes(bytes.len() as u64);
    Ok(())
}

fn record_ending(record: &[u8]) -> &[u8] {
    if record.ends_with(b"\r\n") {
        b"\r\n"
    } else if record.ends_with(b"\n") {
        b"\n"
    } else {
        b""
    }
}

fn serialize_fields(
    fields: &[Vec<u8>],
    delimiter: u8,
    ending: &[u8],
    max_record_bytes: usize,
) -> Result<Vec<u8>, QuarryError> {
    let serialized_len = fields.iter().enumerate().fold(
        ending.len().saturating_add(fields.len().saturating_sub(1)),
        |length, (column, field)| {
            length.saturating_add(delimited_field_len(
                field,
                delimiter,
                column == 0 && field.starts_with(UTF8_BOM)
                    || fields.len() == 1 && field.is_empty() && ending.is_empty(),
            ))
        },
    );
    if serialized_len > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }
    let mut output = Vec::with_capacity(serialized_len);
    for (column, field) in fields.iter().enumerate() {
        if column > 0 {
            output.push(delimiter);
        }
        write_delimited_field(
            &mut output,
            field,
            delimiter,
            column == 0 && field.starts_with(UTF8_BOM)
                || fields.len() == 1 && field.is_empty() && ending.is_empty(),
        );
    }
    output.extend_from_slice(ending);
    Ok(output)
}

fn delimited_field_len(field: &[u8], delimiter: u8, force_quotes: bool) -> usize {
    let quotes = field.iter().filter(|byte| **byte == b'"').count();
    if force_quotes
        || quotes > 0
        || field
            .iter()
            .any(|byte| matches!(*byte, b'\r' | b'\n') || *byte == delimiter)
    {
        field.len().saturating_add(quotes).saturating_add(2)
    } else {
        field.len()
    }
}

fn write_delimited_field(output: &mut Vec<u8>, field: &[u8], delimiter: u8, force_quotes: bool) {
    let needs_quotes = force_quotes
        || field
            .iter()
            .any(|byte| matches!(*byte, b'"' | b'\r' | b'\n') || *byte == delimiter);
    if !needs_quotes {
        output.extend_from_slice(field);
        return;
    }
    output.push(b'"');
    for byte in field {
        output.push(*byte);
        if *byte == b'"' {
            output.push(b'"');
        }
    }
    output.push(b'"');
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::{HeaderMode, OpenOptions as QuarryOpenOptions};

    static NEXT_CASE: AtomicU64 = AtomicU64::new(0);

    fn case() -> PathBuf {
        let id = NEXT_CASE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("quarry-sort-test-{}-{id}", std::process::id()));
        fs::create_dir(&path).unwrap();
        path
    }

    fn session(path: &Path, header_mode: HeaderMode) -> Session {
        Session::open(
            path,
            QuarryOpenOptions {
                rows: 2,
                delimiter: Some(b','),
                header_mode,
                sample_bytes: 64,
                bootstrap_limit: 1024 * 1024,
            },
        )
        .unwrap()
    }

    fn start_custom(
        session: &Session,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        spec: SortSpec,
        destination: &Path,
        config: SortConfig,
    ) -> SortJob {
        SortJob::start(
            session.path.clone(),
            session.file_size,
            session.dialect.delimiter,
            session.dialect.has_header,
            header_renames,
            cell_edits,
            spec,
            destination.to_path_buf(),
            session.source_stamp.clone(),
            config,
        )
        .unwrap()
    }

    fn wait_done(job: &SortJob) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !job.progress().done {
            assert!(Instant::now() < deadline, "sort did not finish");
            thread::yield_now();
        }
    }

    fn sort_artifacts(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".quarry-sort-") || name.starts_with(".quarry-export-")
                })
            })
            .collect()
    }

    fn tiny_config() -> SortConfig {
        SortConfig {
            chunk_bytes: 3,
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
            run_memory_bytes: 40,
            merge_fan_in: 2,
        }
    }

    #[test]
    fn ascending_multipass_sort_applies_overlays_and_keeps_header_multiline_and_ragged_rows() {
        let directory = case();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        let source_bytes = b"\xEF\xBB\xBFid,note,key\r\n1,\"first\nline\",b\r\n2,\"quoted \"\"value\"\"\",a\r\n3,ragged\r\n4,last,a";
        let expected = b"\xEF\xBB\xBFID,note,key\r\n3,ragged\r\n1,\"first\nline\",a\r\n2,\"changed,comma\",a\r\n4,last,a";
        fs::write(&source, source_bytes).unwrap();
        let source_session = session(&source, HeaderMode::FirstRow);
        let job = start_custom(
            &source_session,
            BTreeMap::from([(0, b"ID".to_vec())]),
            BTreeMap::from([((1, 2), b"a".to_vec()), ((2, 1), b"changed,comma".to_vec())]),
            SortSpec {
                column: 2,
                direction: SortDirection::Ascending,
            },
            &destination,
            tiny_config(),
        );

        wait_done(&job);
        let progress = job.progress();
        let SortOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("sort unexpectedly cancelled");
        };
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(summary.rows_sorted, 4);
        assert!(summary.runs_created >= 6, "multipass merge was not forced");
        assert_eq!(summary.bytes_written, expected.len() as u64);
        assert_eq!(progress.bytes_written, summary.bytes_written);
        assert_eq!(progress.bytes_scanned, source_bytes.len() as u64);
        assert_eq!(progress.rows_sorted, 4);
        assert_eq!(summary.header_rows, 1);
        assert_eq!(progress.header_rows, summary.header_rows);
        assert_eq!(summary.merge_passes, 2);
        assert_eq!(progress.merge_passes, summary.merge_passes);
        assert_eq!(progress.peak_temporary_bytes, summary.peak_temporary_bytes);
        assert!(summary.peak_temporary_bytes > summary.bytes_written);
        assert_eq!(progress.elapsed, summary.elapsed);
        assert!(progress.cancellation_latency.is_none());
        assert!(summary.record_multiset_verified);
        assert!(summary.stable_ties_verified);
        assert!(sort_artifacts(&directory).is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn descending_sort_preserves_tie_order_bom_and_valid_record_boundaries() {
        let directory = case();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        let source_bytes = b"\xEF\xBB\xBFone,b\ntwo,a\nthree,b";
        let expected = b"\xEF\xBB\xBFone,b\nthree,b\ntwo,a\n";
        fs::write(&source, source_bytes).unwrap();
        let source_session = session(&source, HeaderMode::NoHeader);
        let mut config = tiny_config();
        config.run_memory_bytes = 1;
        let job = start_custom(
            &source_session,
            BTreeMap::new(),
            BTreeMap::new(),
            SortSpec {
                column: 1,
                direction: SortDirection::Descending,
            },
            &destination,
            config,
        );

        wait_done(&job);
        assert!(matches!(job.wait().unwrap(), SortOutcome::Complete(_)));
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert!(sort_artifacts(&directory).is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn quoted_multiline_keys_sort_by_decoded_value_and_keep_ties_stable() {
        let directory = case();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        let source_bytes = b"id,key\n1,\"z\nline\"\n2,\"a,comma\"\n3,\n4,\"z\nline\"\n";
        let expected = b"id,key\n3,\n2,\"a,comma\"\n1,\"z\nline\"\n4,\"z\nline\"\n";
        fs::write(&source, source_bytes).unwrap();
        let source_session = session(&source, HeaderMode::FirstRow);
        let job = start_custom(
            &source_session,
            BTreeMap::new(),
            BTreeMap::new(),
            SortSpec {
                column: 1,
                direction: SortDirection::Ascending,
            },
            &destination,
            tiny_config(),
        );

        wait_done(&job);
        assert!(matches!(job.wait().unwrap(), SortOutcome::Complete(_)));
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(sort_artifacts(&directory).is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn output_verifier_rejects_a_dropped_record_replaced_by_a_duplicate() {
        let mut expected = RecordMultisetFingerprint::default();
        for record in [b"alpha\n".as_slice(), b"bravo\n", b"charlie"] {
            expected.observe(record);
        }
        let mut verifier = OutputVerifier::new(expected);
        verifier.observe(b"alpha\n", b"").unwrap();
        verifier.observe(b"bravo\n", b"").unwrap();
        verifier.observe(b"bravo\n", b"").unwrap();

        let error = verifier.finish().unwrap_err();
        assert!(matches!(
            error,
            QuarryError::Io(ref error) if error.kind() == io::ErrorKind::InvalidData
        ));
        assert!(
            error
                .to_string()
                .contains("did not preserve the effective record multiset")
        );
    }

    #[test]
    fn output_verifier_allows_only_a_record_terminator_difference() {
        let mut expected = RecordMultisetFingerprint::default();
        expected.observe(b"unterminated");
        let mut verifier = OutputVerifier::new(expected);
        verifier.observe(b"unterminated", b"\r\n").unwrap();

        let verification = verifier.finish().unwrap();
        assert!(verification.record_multiset_verified);
        assert!(verification.stable_ties_verified);
    }

    #[test]
    fn adjacent_tie_verifier_rejects_reordered_equal_key_ordinals() {
        let mut verifier = OutputVerifier::new(RecordMultisetFingerprint::default());
        let error = verifier
            .verify_adjacent(b"same", 1, b"same", 0)
            .unwrap_err();
        assert!(matches!(
            error,
            QuarryError::Io(ref error) if error.kind() == io::ErrorKind::InvalidData
        ));
        assert!(error.to_string().contains("changed equal-key row order"));
    }

    #[test]
    fn output_verifier_rejects_a_missing_adjacency_check() {
        let mut expected = RecordMultisetFingerprint::default();
        expected.observe(b"first\n");
        expected.observe(b"second\n");
        let mut verifier = OutputVerifier::new(expected);
        verifier.observe(b"first\n", b"").unwrap();
        verifier.observe(b"second\n", b"").unwrap();

        let error = verifier.finish().unwrap_err();
        assert!(matches!(
            error,
            QuarryError::Io(ref error) if error.kind() == io::ErrorKind::InvalidData
        ));
        assert!(
            error
                .to_string()
                .contains("did not verify every adjacent row")
        );
    }

    #[test]
    fn initial_run_flush_cancellation_propagates_as_cancelled() {
        let directory = case();
        let destination = directory.join("sorted.csv");
        let workspace = RunWorkspace::create(&destination).unwrap();
        let shared = SharedState::new(4);
        let header_renames = BTreeMap::new();
        let cell_edits = BTreeMap::new();
        let mut builder = InitialRunBuilder::new(
            b',',
            false,
            &header_renames,
            &cell_edits,
            SortSpec {
                column: 0,
                direction: SortDirection::Ascending,
            },
            false,
            SortConfig {
                run_memory_bytes: 1024,
                ..tiny_config()
            },
            &workspace,
            &shared,
        );
        assert_eq!(builder.process(b"b\n", 0).unwrap(), BuilderStep::Complete);
        assert_eq!(builder.process(b"a\n", 1).unwrap(), BuilderStep::Complete);
        shared.request_cancel();

        assert!(matches!(builder.finish(2).unwrap(), ScanOutcome::Cancelled));
        workspace.cleanup().unwrap();
        assert!(sort_artifacts(&directory).is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cancellation_removes_private_output_and_all_sort_runs() {
        let directory = case();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        fs::write(&source, b"value,key\n".repeat(100_000)).unwrap();
        let source_session = session(&source, HeaderMode::NoHeader);
        let job = start_custom(
            &source_session,
            BTreeMap::new(),
            BTreeMap::new(),
            SortSpec {
                column: 1,
                direction: SortDirection::Ascending,
            },
            &destination,
            SortConfig {
                chunk_bytes: 1,
                ..tiny_config()
            },
        );
        job.cancel();

        wait_done(&job);
        let progress = job.progress();
        let cancellation_latency = progress
            .cancellation_latency
            .expect("cancelled sort should report cancellation latency");
        assert!(progress.elapsed >= cancellation_latency);
        assert!(progress.done);
        assert!(progress.cancelled);
        assert_eq!(job.wait().unwrap(), SortOutcome::Cancelled);
        assert!(!destination.exists());
        assert!(sort_artifacts(&directory).is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn inserted_line_ending_cannot_exceed_the_output_record_limit() {
        let directory = case();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        fs::write(&source, b"z\naaaa,a").unwrap();
        let source_session = session(&source, HeaderMode::NoHeader);
        let job = start_custom(
            &source_session,
            BTreeMap::new(),
            BTreeMap::new(),
            SortSpec {
                column: 0,
                direction: SortDirection::Ascending,
            },
            &destination,
            SortConfig {
                chunk_bytes: 1,
                max_record_bytes: 6,
                run_memory_bytes: 1,
                merge_fan_in: 2,
            },
        );

        wait_done(&job);
        assert!(matches!(
            job.wait(),
            Err(QuarryError::RecordTooLarge { limit: 6 })
        ));
        assert!(!destination.exists());
        assert!(sort_artifacts(&directory).is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn source_change_before_publication_discards_output_and_runs() {
        let directory = case();
        let source_path = directory.join("source.csv");
        let moved_source = directory.join("opened-source.csv");
        let destination = directory.join("sorted.csv");
        let source_bytes = b"id,key\n1,b\n2,a\n";
        fs::write(&source_path, source_bytes).unwrap();
        let source_session = session(&source_path, HeaderMode::FirstRow);
        let mut source = File::open(&source_path).unwrap();
        let output = ExportTarget::new_private_guarded(
            &source_path,
            destination.clone(),
            &source,
            source_session.source_stamp.clone(),
        )
        .unwrap();
        fs::rename(&source_path, &moved_source).unwrap();
        fs::write(&source_path, b"external,change\n").unwrap();
        let shared = SharedState::new(source_bytes.len() as u64);

        let result = run_sort(
            &mut source,
            &source_path,
            &source_session.source_stamp,
            b',',
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            SortSpec {
                column: 1,
                direction: SortDirection::Ascending,
            },
            destination.clone(),
            output,
            false,
            tiny_config(),
            &shared,
        );

        assert!(matches!(result, Err(QuarryError::SourceChanged)));
        assert_eq!(fs::read(&moved_source).unwrap(), source_bytes);
        assert_eq!(fs::read(&source_path).unwrap(), b"external,change\n");
        assert!(!destination.exists());
        assert!(sort_artifacts(&directory).is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sort_runs_and_working_copy_are_owner_only() {
        let directory = case();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        let workspace = RunWorkspace::create(&destination).unwrap();
        assert_eq!(
            fs::metadata(&workspace.path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let (run_path, run) = workspace.create_run().unwrap();
        assert_eq!(
            fs::metadata(run_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(run);
        workspace.cleanup().unwrap();
        fs::write(&source, b"one,b\ntwo,a\n").unwrap();
        let source_session = session(&source, HeaderMode::NoHeader);
        let job = start_custom(
            &source_session,
            BTreeMap::new(),
            BTreeMap::new(),
            SortSpec {
                column: 1,
                direction: SortDirection::Ascending,
            },
            &destination,
            tiny_config(),
        );

        wait_done(&job);
        assert!(matches!(job.wait().unwrap(), SortOutcome::Complete(_)));
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(sort_artifacts(&directory).is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn temporary_disk_estimate_covers_two_generations_of_framed_keyed_runs() {
        assert_eq!(estimate_sort_temporary_bytes(123, 7), 828);
        assert_eq!(estimate_sort_temporary_bytes(u64::MAX, 1), u64::MAX);
        assert_eq!(estimate_sort_temporary_bytes(1, u64::MAX), u64::MAX);
    }

    #[test]
    fn merge_fan_in_keeps_every_retained_payload_within_the_memory_budget() {
        let fan_in = effective_merge_fan_in(DEFAULT_SORT_CONFIG);
        assert_eq!(fan_in, 2);
        let heap_keys = fan_in.saturating_mul(DEFAULT_SORT_CONFIG.max_record_bytes);
        let pending_key = DEFAULT_SORT_CONFIG.max_record_bytes;
        let pending_record = DEFAULT_SORT_CONFIG.max_record_bytes;
        let retained_payload = heap_keys
            .saturating_add(pending_key)
            .saturating_add(pending_record);
        assert_eq!(retained_payload, MERGE_MEMORY_BUDGET_BYTES);
    }
}
