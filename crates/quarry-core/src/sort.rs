use std::cmp::Ordering as CmpOrdering;
use std::collections::hash_map::{DefaultHasher, RandomState};
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
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
    CaseSensitivity, DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, FilterExportOutcome,
    QuarryError, Session, SourceStamp,
};

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
const DEFAULT_RUN_MEMORY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MERGE_FAN_IN: usize = 32;
// A numeric key adds a sign, an eight-byte decimal order, and a terminator.
const NUMERIC_KEY_OVERHEAD: usize = 10;
const MAX_NUMERIC_EXPONENT: i64 = 1_000_000;
// At minimum fan-in, retain two heap keys, a pending key, and one record.
const MERGE_MEMORY_BUDGET_BYTES: usize = 256 * 1024 * 1024 + 3 * NUMERIC_KEY_OVERHEAD;
static NEXT_SORT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Text,
    Number,
    CharacterCount,
    WordCount,
    Shuffle { seed: u64 },
    Reverse,
}

impl SortMode {
    pub fn shuffle() -> Self {
        Self::Shuffle {
            seed: RandomState::new().hash_one(()),
        }
    }

    pub const fn uses_column(self) -> bool {
        !matches!(self, Self::Shuffle { .. } | Self::Reverse)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub column: usize,
    pub mode: SortMode,
    pub direction: SortDirection,
    pub case_sensitivity: CaseSensitivity,
}

impl SortSpec {
    /// Return an ascending comparison key. Numeric values use exact decimal order;
    /// equal values share a key, and blank numeric fields have an empty key.
    /// Numbers accept ASCII whitespace, a sign, a decimal point, and an optional
    /// exponent from -1,000,000 to 1,000,000. Other nonblank values return an error.
    /// Character counts use Unicode scalar values, including combining marks;
    /// word counts split at Unicode whitespace. `ordinal` is the zero-based source
    /// data-row position and determines Shuffle and Reverse keys.
    pub fn key(&self, value: &[u8], ordinal: u64) -> Result<Vec<u8>, QuarryError> {
        match self.mode {
            SortMode::Text => {
                let mut key = value.to_vec();
                if self.case_sensitivity == CaseSensitivity::Insensitive {
                    key.make_ascii_lowercase();
                }
                Ok(key)
            }
            SortMode::Number => numeric_key(value),
            SortMode::CharacterCount | SortMode::WordCount => {
                let value = std::str::from_utf8(value)
                    .map_err(|_| invalid_sort_output("expected valid UTF-8 text"))?;
                let count = if self.mode == SortMode::CharacterCount {
                    // ponytail: count Unicode scalars, add grapheme segmentation if visual character counts are needed.
                    value.chars().count()
                } else {
                    value.split_whitespace().count()
                };
                Ok((count as u64).to_be_bytes().to_vec())
            }
            SortMode::Shuffle { seed } => {
                let mut hasher = DefaultHasher::new();
                hasher.write_u64(seed);
                hasher.write_u64(ordinal);
                Ok(hasher.finish().to_be_bytes().to_vec())
            }
            SortMode::Reverse => Ok((!ordinal).to_be_bytes().to_vec()),
        }
    }
}

fn numeric_key(value: &[u8]) -> Result<Vec<u8>, QuarryError> {
    let value = value.trim_ascii();
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let (negative, value) = match value[0] {
        b'-' => (true, &value[1..]),
        b'+' => (false, &value[1..]),
        _ => (false, value),
    };
    let (mantissa, exponent) = match value.iter().position(|byte| matches!(byte, b'e' | b'E')) {
        Some(index) => {
            let exponent = std::str::from_utf8(&value[index + 1..])
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|value| (-MAX_NUMERIC_EXPONENT..=MAX_NUMERIC_EXPONENT).contains(value))
                .ok_or_else(|| {
                    invalid_sort_output(
                        "numeric exponent must be an integer from -1000000 to 1000000",
                    )
                })?;
            (&value[..index], exponent)
        }
        None => (value, 0),
    };
    let mut digits = 0_usize;
    let mut integer_digits = None;
    let mut leading_zeros = 0_usize;
    let mut first_nonzero = None;
    let mut last_nonzero = 0;
    for (index, &byte) in mantissa.iter().enumerate() {
        match byte {
            b'0'..=b'9' => {
                digits += 1;
                if byte != b'0' {
                    first_nonzero.get_or_insert(index);
                    last_nonzero = index;
                } else if first_nonzero.is_none() {
                    leading_zeros += 1;
                }
            }
            b'.' if integer_digits.is_none() => integer_digits = Some(digits),
            _ => {
                return Err(invalid_sort_output(
                    "expected a number with an optional sign, decimal point, and exponent",
                ));
            }
        }
    }
    if digits == 0 {
        return Err(invalid_sort_output("expected at least one numeric digit"));
    }
    let Some(first_nonzero) = first_nonzero else {
        return Ok(vec![2]);
    };
    let order = i64::try_from(integer_digits.unwrap_or(digits))
        .ok()
        .and_then(|value| value.checked_sub(i64::try_from(leading_zeros).ok()?))
        .and_then(|value| value.checked_add(exponent))
        .ok_or_else(|| invalid_sort_output("numeric value is too long"))?;
    let significant = &mantissa[first_nonzero..=last_nonzero];
    let capacity = significant
        .len()
        .checked_add(NUMERIC_KEY_OVERHEAD)
        .ok_or_else(|| invalid_sort_output("numeric value is too long"))?;
    let mut key = Vec::with_capacity(capacity);
    key.push(if negative { 1 } else { 3 });
    key.extend_from_slice(&((order as u64) ^ (1 << 63)).to_be_bytes());
    key.extend(significant.iter().copied().filter(|byte| *byte != b'.'));
    // The terminator makes complementing negative keys reverse prefix order too.
    key.push(0);
    if negative {
        for byte in &mut key[1..] {
            *byte = !*byte;
        }
    }
    Ok(key)
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
        .saturating_add(data_rows.saturating_mul(48 + 2 * NUMERIC_KEY_OVERHEAD as u64))
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
        if !spec.mode.uses_column() && spec.direction != SortDirection::Ascending {
            return Err(QuarryError::InvalidOption(
                "Shuffle and Reverse do not accept a sort direction",
            ));
        }
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
    if config
        .max_record_bytes
        .saturating_mul(4)
        .saturating_add(3 * NUMERIC_KEY_OVERHEAD)
        > MERGE_MEMORY_BUDGET_BYTES
    {
        return Err(QuarryError::InvalidOption(
            "sort record limit exceeds the merge memory budget",
        ));
    }
    Ok(())
}

fn effective_merge_fan_in(config: SortConfig, max_key_bytes: usize) -> usize {
    let key_bytes = max_key_bytes.max(1);
    let key_slots = MERGE_MEMORY_BUDGET_BYTES.saturating_sub(config.max_record_bytes) / key_bytes;
    config.merge_fan_in.min(key_slots.saturating_sub(1).max(2))
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
    max_key_bytes: usize,
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
    max_key_bytes: usize,
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
            max_key_bytes: 0,
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
        let value = if self.spec.mode.uses_column() {
            row_edits
                .iter()
                .find(|(column, _)| *column == self.spec.column)
                .map(|(_, value)| *value)
                .or_else(|| fields.get(self.spec.column).map(|field| field.as_ref()))
                .unwrap_or_default()
        } else {
            &[]
        };
        let key = self
            .spec
            .key(value, self.rows)
            .map_err(|error| match self.spec.mode {
                SortMode::Number => QuarryError::InvalidNumericSortValue {
                    data_row: self.rows.saturating_add(1),
                    column: self.spec.column.saturating_add(1),
                },
                SortMode::CharacterCount | SortMode::WordCount => {
                    QuarryError::InvalidUtf8SortValue {
                        data_row: self.rows.saturating_add(1),
                        column: self.spec.column.saturating_add(1),
                    }
                }
                _ => error,
            })?;
        self.max_key_bytes = self.max_key_bytes.max(key.len());
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
            max_key_bytes: self.max_key_bytes,
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

    let merge_fan_in = effective_merge_fan_in(config, scan.max_key_bytes);
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
        .filter(|length| *length <= max_record_bytes.saturating_add(NUMERIC_KEY_OVERHEAD))
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

    fn number_spec(direction: SortDirection) -> SortSpec {
        SortSpec {
            column: 1,
            mode: SortMode::Number,
            direction,
            case_sensitivity: CaseSensitivity::Sensitive,
        }
    }

    #[test]
    fn count_sorts_use_unicode_decoded_text_edits_and_stable_ties() {
        let directory = case();
        let source = directory.join("source.csv");
        let rows = [
            "a,é\n",
            "b,😀\n",
            "c,e\u{301}\n",
            "d,a b\n",
            "e,\n",
            "f\n",
            "g,  \t\n",
            "h,\"one\n two\"\n",
            "i,z\u{2003}y\n",
            "j,old\n",
        ];
        let source_bytes = format!("id,value\n{}", rows.concat());
        fs::write(&source, &source_bytes).unwrap();
        let source_session = session(&source, HeaderMode::FirstRow);
        for (mode, direction, expected_order) in [
            (
                SortMode::CharacterCount,
                SortDirection::Ascending,
                "efabcdgijh",
            ),
            (
                SortMode::CharacterCount,
                SortDirection::Descending,
                "hjdgicabef",
            ),
            (SortMode::WordCount, SortDirection::Ascending, "efgabcdhij"),
            (SortMode::WordCount, SortDirection::Descending, "dhijabcefg"),
        ] {
            let destination = directory.join(format!("{mode:?}-{direction:?}.csv"));
            let job = start_custom(
                &source_session,
                BTreeMap::new(),
                BTreeMap::from([((10, 1), "猫 dog".as_bytes().to_vec())]),
                SortSpec {
                    mode,
                    ..number_spec(direction)
                },
                &destination,
                SortConfig {
                    run_memory_bytes: 1,
                    ..tiny_config()
                },
            );
            wait_done(&job);
            let SortOutcome::Complete(summary) = job.wait().unwrap() else {
                panic!("sort unexpectedly cancelled");
            };
            let expected = format!(
                "id,value\n{}",
                expected_order
                    .bytes()
                    .map(|id| {
                        if id == b'j' {
                            "j,猫 dog\n"
                        } else {
                            rows[(id - b'a') as usize]
                        }
                    })
                    .collect::<String>()
            );
            assert_eq!(
                fs::read(&destination).unwrap(),
                expected.as_bytes(),
                "{mode:?} {direction:?}"
            );
            assert!(summary.merge_passes > 2);
            assert!(summary.stable_ties_verified && summary.record_multiset_verified);
            assert!(sort_artifacts(&directory).is_empty());
        }
        assert_eq!(fs::read(&source).unwrap(), source_bytes.as_bytes());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn count_sorts_reject_invalid_utf8_with_data_coordinates_and_cleanup() {
        for mode in [SortMode::CharacterCount, SortMode::WordCount] {
            let directory = case();
            let source = directory.join("source.csv");
            let destination = directory.join("sorted.csv");
            let source_bytes = b"id,value,other\na,word,\xff\nb,words,\xff\nc,\xff,x\n";
            fs::write(&source, source_bytes).unwrap();
            let source_session = session(&source, HeaderMode::FirstRow);
            let job = start_custom(
                &source_session,
                BTreeMap::new(),
                BTreeMap::new(),
                SortSpec {
                    mode,
                    ..number_spec(SortDirection::Ascending)
                },
                &destination,
                SortConfig {
                    run_memory_bytes: 1,
                    ..tiny_config()
                },
            );
            wait_done(&job);
            assert!(job.progress().runs_created > 0);
            assert_eq!(
                job.error().unwrap(),
                "Cannot count text at data row 3, column 2: expected valid UTF-8 text."
            );
            assert!(matches!(
                job.wait(),
                Err(QuarryError::InvalidUtf8SortValue {
                    data_row: 3,
                    column: 2
                })
            ));
            assert!(!destination.exists());
            assert!(sort_artifacts(&directory).is_empty());
            assert_eq!(fs::read(&source).unwrap(), source_bytes);
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn reverse_multipass_preserves_bom_header_edits_and_record_boundaries() {
        for header_mode in [HeaderMode::FirstRow, HeaderMode::NoHeader] {
            let directory = case();
            let source = directory.join("source.csv");
            let destination = directory.join("reversed.csv");
            let has_header = header_mode == HeaderMode::FirstRow;
            let mut source_bytes = UTF8_BOM.to_vec();
            if has_header {
                source_bytes.extend_from_slice(b"id,value\r\n");
            }
            source_bytes.extend_from_slice(b"a,\xff\r\nb,\"two\r\nlines\"\r\nc,same\r\nd,same");
            fs::write(&source, &source_bytes).unwrap();
            let source_session = session(&source, header_mode);
            let job = start_custom(
                &source_session,
                if has_header {
                    BTreeMap::from([(0, b"ID".to_vec())])
                } else {
                    BTreeMap::new()
                },
                BTreeMap::from([((1 + u64::from(has_header), 0), b"B".to_vec())]),
                SortSpec {
                    mode: SortMode::Reverse,
                    column: usize::MAX,
                    ..number_spec(SortDirection::Ascending)
                },
                &destination,
                SortConfig {
                    run_memory_bytes: 1,
                    ..tiny_config()
                },
            );
            wait_done(&job);
            let SortOutcome::Complete(summary) = job.wait().unwrap() else {
                panic!("sort unexpectedly cancelled");
            };
            let mut expected = UTF8_BOM.to_vec();
            if has_header {
                expected.extend_from_slice(b"ID,value\r\n");
            }
            expected.extend_from_slice(b"d,same\r\nc,same\r\nB,\"two\r\nlines\"\r\na,\xff\r\n");
            assert_eq!(fs::read(&destination).unwrap(), expected);
            assert_eq!(fs::read(&source).unwrap(), source_bytes);
            assert_eq!(summary.rows_sorted, 4);
            assert_eq!(summary.header_rows, u64::from(has_header));
            assert!(summary.merge_passes > 1);
            assert!(summary.stable_ties_verified && summary.record_multiset_verified);
            assert!(sort_artifacts(&directory).is_empty());
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn seeded_shuffle_is_a_repeatable_multipass_permutation() {
        let directory = case();
        let source = directory.join("source.csv");
        let rows = (0..64).map(|id| format!("{id},same\n")).collect::<Vec<_>>();
        let source_bytes = format!("id,value\n{}", rows.concat());
        fs::write(&source, &source_bytes).unwrap();
        let source_session = session(&source, HeaderMode::FirstRow);
        let mut outputs = Vec::new();
        for (attempt, seed) in [17, 17, 18].into_iter().enumerate() {
            let destination = directory.join(format!("shuffle-{attempt}.csv"));
            let job = start_custom(
                &source_session,
                BTreeMap::new(),
                BTreeMap::new(),
                SortSpec {
                    mode: SortMode::Shuffle { seed },
                    column: usize::MAX,
                    ..number_spec(SortDirection::Ascending)
                },
                &destination,
                SortConfig {
                    run_memory_bytes: 1,
                    ..tiny_config()
                },
            );
            wait_done(&job);
            let SortOutcome::Complete(summary) = job.wait().unwrap() else {
                panic!("sort unexpectedly cancelled");
            };
            let output = fs::read_to_string(&destination).unwrap();
            let mut actual_rows = output.lines().skip(1).collect::<Vec<_>>();
            actual_rows.sort_unstable();
            let mut expected_rows = rows.iter().map(|row| row.trim_end()).collect::<Vec<_>>();
            expected_rows.sort_unstable();
            assert_eq!(actual_rows, expected_rows);
            assert!(output.starts_with("id,value\n"));
            assert!(summary.merge_passes > 2);
            assert!(summary.stable_ties_verified && summary.record_multiset_verified);
            assert!(
                summary.peak_temporary_bytes
                    <= estimate_sort_temporary_bytes(source_bytes.len() as u64, rows.len() as u64)
            );
            assert!(sort_artifacts(&directory).is_empty());
            outputs.push(output);
        }
        assert_eq!(outputs[0], outputs[1]);
        assert_ne!(outputs[0], outputs[2]);
        assert_ne!(outputs[0], source_bytes);
        assert_eq!(fs::read(&source).unwrap(), source_bytes.as_bytes());
        for mode in [SortMode::shuffle(), SortMode::Reverse] {
            assert!(!mode.uses_column());
            let destination = directory.join("invalid-direction.csv");
            let result = source_session.start_create_sorted_working_copy(
                BTreeMap::new(),
                BTreeMap::new(),
                SortSpec {
                    mode,
                    ..number_spec(SortDirection::Descending)
                },
                &destination,
            );
            assert!(matches!(
                result,
                Err(QuarryError::InvalidOption(
                    "Shuffle and Reverse do not accept a sort direction"
                ))
            ));
            assert!(!destination.exists());
            assert!(sort_artifacts(&directory).is_empty());
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn numeric_keys_are_exact_canonical_and_bounded() {
        let spec = number_spec(SortDirection::Ascending);
        for equivalents in [
            ["2", "02", "2.0", "+2e0", "  200e-2\t"],
            ["0", "-0", "+0.00", "0e1000000", "0e-1000000"],
            ["-0.01", "-.01", "-1e-2", "-0.0100", "-00001E-2"],
        ] {
            let expected = spec.key(equivalents[0].as_bytes(), 0).unwrap();
            for value in equivalents {
                assert_eq!(spec.key(value.as_bytes(), 0).unwrap(), expected, "{value}");
            }
        }
        let ascending = [
            "",
            "-1e1000000",
            "-9007199254740993",
            "-9007199254740992",
            "-10",
            "-1.01",
            "-1",
            "-.01",
            "-1e-1000000",
            "0",
            "1e-1000000",
            ".01",
            "1.",
            "1.00000000000000000001",
            "1.00000000000000000002",
            "2",
            "10",
            "9007199254740992",
            "9007199254740993",
            "1e1000000",
        ];
        for pair in ascending.windows(2) {
            assert!(
                spec.key(pair[0].as_bytes(), 0).unwrap() < spec.key(pair[1].as_bytes(), 0).unwrap(),
                "{pair:?}"
            );
        }
        assert!(spec.key(b" \t\r\n", 0).unwrap().is_empty());
        assert_eq!(spec.key(b"1e1000000", 0).unwrap().len(), 11);
        for invalid in [
            "+",
            "-",
            ".",
            "1..2",
            "1 2",
            "1,000",
            "$2",
            "NaN",
            "inf",
            "-Infinity",
            "1e",
            "1e+",
            "1ee2",
            "1e1.5",
            "1e1000001",
            "1e-1000001",
            "1e999999999999999999999",
            "0e1000001",
            "\u{a0}2",
        ] {
            assert!(spec.key(invalid.as_bytes(), 0).is_err(), "{invalid}");
        }
        assert!(spec.key(b"\xff", 0).is_err());
        let long_number = vec![b'1'; 1024 * 1024];
        assert_eq!(
            spec.key(&long_number, 0).unwrap().len(),
            long_number.len() + NUMERIC_KEY_OVERHEAD
        );
    }

    #[test]
    fn numeric_multipass_sort_is_exact_stable_and_applies_sparse_edits() {
        let directory = case();
        let source = directory.join("source.csv");
        let rows = [
            "a,10\n",
            "b,2\n",
            "c,02\n",
            "d,2.0\n",
            "e,+2e0\n",
            "f,-0\n",
            "g,+0.00\n",
            "h,0e1000000\n",
            "i,9007199254740993\n",
            "j,9007199254740992\n",
            "k,-1.01\n",
            "l,-1\n",
            "m,-.01\n",
            "n,.01\n",
            "o,\n",
            "p, \t \n",
            "q\n",
            "r,replace me\n",
            "s,1e1000000\n",
            "t,1e-1000000\n",
            "u,-1e1000000\n",
            "v,-1e-1000000\n",
            "w,1.00000000000000000001\n",
            "x,1.00000000000000000002\n",
            "y,1.0\n",
        ];
        let source_bytes = format!("\u{feff}id,key\n{}", rows.concat());
        fs::write(&source, &source_bytes).unwrap();
        let source_session = session(&source, HeaderMode::FirstRow);
        for (direction, expected_order) in [
            (SortDirection::Ascending, "opqurklmvfghtnywxbcdeajis"),
            (SortDirection::Descending, "sijabcdexwyntfghvmlkruopq"),
        ] {
            let destination = directory.join(format!("{direction:?}.csv"));
            let job = start_custom(
                &source_session,
                BTreeMap::from([(0, b"ID".to_vec())]),
                BTreeMap::from([((18, 1), b"-2.5".to_vec())]),
                number_spec(direction),
                &destination,
                SortConfig {
                    run_memory_bytes: 1,
                    ..tiny_config()
                },
            );
            wait_done(&job);
            let SortOutcome::Complete(summary) = job.wait().unwrap() else {
                panic!("sort unexpectedly cancelled");
            };
            let expected = format!(
                "\u{feff}ID,key\n{}",
                expected_order
                    .bytes()
                    .map(|id| {
                        if id == b'r' {
                            "r,-2.5\n"
                        } else {
                            rows[(id - b'a') as usize]
                        }
                    })
                    .collect::<String>()
            );
            assert_eq!(
                fs::read(&destination).unwrap(),
                expected.as_bytes(),
                "{direction:?}"
            );
            assert_eq!(summary.rows_sorted, rows.len() as u64);
            assert!(summary.merge_passes > 2);
            assert!(summary.stable_ties_verified);
            assert!(summary.record_multiset_verified);
            assert!(
                summary.peak_temporary_bytes
                    <= estimate_sort_temporary_bytes(source_bytes.len() as u64, rows.len() as u64)
            );
            assert!(sort_artifacts(&directory).is_empty());
        }
        assert_eq!(fs::read(&source).unwrap(), source_bytes.as_bytes());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_numeric_edits_report_data_coordinates_and_remove_sort_artifacts() {
        for header_mode in [HeaderMode::FirstRow, HeaderMode::NoHeader] {
            let directory = case();
            let source = directory.join("source.csv");
            let destination = directory.join("sorted.csv");
            let header = if header_mode == HeaderMode::FirstRow {
                "id,key\n"
            } else {
                ""
            };
            let source_bytes = format!("{header}a,3\nb,2\nc,1\n");
            fs::write(&source, &source_bytes).unwrap();
            let source_session = session(&source, header_mode);
            let job = start_custom(
                &source_session,
                BTreeMap::new(),
                BTreeMap::from([((2 + u64::from(!header.is_empty()), 1), b"ten".to_vec())]),
                number_spec(SortDirection::Ascending),
                &destination,
                SortConfig {
                    run_memory_bytes: 1,
                    ..tiny_config()
                },
            );
            wait_done(&job);
            assert!(job.progress().runs_created > 0);
            assert_eq!(
                job.error().unwrap(),
                "Cannot sort as Number at data row 3, column 2: expected a number (for example -12.5 or 1e3)."
            );
            assert!(matches!(
                job.wait(),
                Err(QuarryError::InvalidNumericSortValue {
                    data_row: 3,
                    column: 2
                })
            ));
            assert!(!destination.exists());
            assert!(sort_artifacts(&directory).is_empty());
            assert_eq!(fs::read(&source).unwrap(), source_bytes.as_bytes());
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn numeric_merge_accepts_expanded_keys_and_rejects_oversized_run_keys() {
        let directory = case();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        fs::write(&source, b"9\n1\n8\n2\n").unwrap();
        let source_session = session(&source, HeaderMode::NoHeader);
        let config = SortConfig {
            max_record_bytes: 2,
            run_memory_bytes: 1,
            ..tiny_config()
        };
        let job = start_custom(
            &source_session,
            BTreeMap::new(),
            BTreeMap::new(),
            SortSpec {
                column: 0,
                ..number_spec(SortDirection::Ascending)
            },
            &destination,
            config,
        );
        wait_done(&job);
        let SortOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("sort unexpectedly cancelled");
        };
        assert_eq!(fs::read(&destination).unwrap(), b"1\n2\n8\n9\n");
        assert!(summary.merge_passes > 1);
        assert!(summary.peak_temporary_bytes <= estimate_sort_temporary_bytes(8, 4));
        let workspace = RunWorkspace::create(&destination).unwrap();
        let (path, mut writer) = workspace.create_run().unwrap();
        write_entry_parts(
            &mut writer,
            &vec![0; config.max_record_bytes + NUMERIC_KEY_OVERHEAD + 1],
            b"1\n",
            0,
        )
        .unwrap();
        writer.flush().unwrap();
        let error = read_head(
            &mut BufReader::new(File::open(path).unwrap()),
            config.max_record_bytes,
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid sort key length"));
        workspace.cleanup().unwrap();
        assert!(sort_artifacts(&directory).is_empty());
        assert!(
            validate_config(SortConfig {
                max_record_bytes: DEFAULT_MAX_RECORD_BYTES + 1,
                ..tiny_config()
            })
            .is_err()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn case_insensitive_multipass_sort_keeps_equal_case_variants_stable() {
        let directory = case();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        let source_bytes = b"\xEF\xBB\xBFid,note,key\r\n1,\"first\nline\",B\r\n2,\"quoted \"\"value\"\"\",A\r\n3,ragged\r\n4,last,a";
        let expected = b"\xEF\xBB\xBFID,note,key\r\n3,ragged\r\n1,\"first\nline\",a\r\n2,\"changed,comma\",A\r\n4,last,a";
        fs::write(&source, source_bytes).unwrap();
        let source_session = session(&source, HeaderMode::FirstRow);
        let job = start_custom(
            &source_session,
            BTreeMap::from([(0, b"ID".to_vec())]),
            BTreeMap::from([((1, 2), b"a".to_vec()), ((2, 1), b"changed,comma".to_vec())]),
            SortSpec {
                mode: SortMode::Text,
                column: 2,
                direction: SortDirection::Ascending,
                case_sensitivity: CaseSensitivity::Insensitive,
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
                mode: SortMode::Text,
                column: 1,
                direction: SortDirection::Descending,
                case_sensitivity: CaseSensitivity::Sensitive,
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
                mode: SortMode::Text,
                column: 1,
                direction: SortDirection::Ascending,
                case_sensitivity: CaseSensitivity::Sensitive,
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
                mode: SortMode::Text,
                column: 0,
                direction: SortDirection::Ascending,
                case_sensitivity: CaseSensitivity::Sensitive,
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
    fn initial_run_scan_reports_the_longest_decoded_key() {
        let directory = case();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        fs::write(&source, b"id,key\n1,x\n2,\"long,key\"\n").unwrap();
        let mut source_file = File::open(&source).unwrap();
        let workspace = RunWorkspace::create(&destination).unwrap();
        let shared = SharedState::new(fs::metadata(&source).unwrap().len());

        let outcome = create_initial_runs(
            &mut source_file,
            b',',
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            SortSpec {
                mode: SortMode::Text,
                column: 1,
                direction: SortDirection::Ascending,
                case_sensitivity: CaseSensitivity::Sensitive,
            },
            false,
            SortConfig {
                run_memory_bytes: 1024,
                ..tiny_config()
            },
            &workspace,
            &shared,
        )
        .unwrap();
        let ScanOutcome::Complete(summary) = outcome else {
            panic!("scan unexpectedly cancelled");
        };
        assert_eq!(summary.max_key_bytes, b"long,key".len());

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
                mode: SortMode::shuffle(),
                column: usize::MAX,
                direction: SortDirection::Ascending,
                case_sensitivity: CaseSensitivity::Sensitive,
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
                mode: SortMode::Text,
                column: 0,
                direction: SortDirection::Ascending,
                case_sensitivity: CaseSensitivity::Sensitive,
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
                mode: SortMode::Text,
                column: 1,
                direction: SortDirection::Ascending,
                case_sensitivity: CaseSensitivity::Sensitive,
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
                mode: SortMode::Text,
                column: 1,
                direction: SortDirection::Ascending,
                case_sensitivity: CaseSensitivity::Sensitive,
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
        assert_eq!(estimate_sort_temporary_bytes(123, 7), 968);
        assert_eq!(estimate_sort_temporary_bytes(u64::MAX, 1), u64::MAX);
        assert_eq!(estimate_sort_temporary_bytes(1, u64::MAX), u64::MAX);
    }

    #[test]
    fn observed_key_width_uses_a_wide_stable_merge() {
        let directory = case();
        let source = directory.join("source.csv");
        let destination = directory.join("sorted.csv");
        fs::write(&source, b"value,key\nr1,b\nr2,a\nr3,c\nr4,a\nr5,b\n").unwrap();
        let source_session = session(&source, HeaderMode::FirstRow);
        let job = start_custom(
            &source_session,
            BTreeMap::new(),
            BTreeMap::new(),
            SortSpec {
                mode: SortMode::Text,
                column: 1,
                direction: SortDirection::Ascending,
                case_sensitivity: CaseSensitivity::Sensitive,
            },
            &destination,
            SortConfig {
                run_memory_bytes: 1,
                merge_fan_in: 4,
                ..tiny_config()
            },
        );

        wait_done(&job);
        let SortOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("sort unexpectedly cancelled");
        };
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"value,key\nr2,a\nr4,a\nr1,b\nr5,b\nr3,c\n"
        );
        assert_eq!(summary.merge_passes, 2);
        assert!(summary.stable_ties_verified);
        assert!(sort_artifacts(&directory).is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn merge_fan_in_keeps_every_retained_payload_within_the_memory_budget() {
        assert_eq!(
            effective_merge_fan_in(DEFAULT_SORT_CONFIG, 32),
            DEFAULT_MERGE_FAN_IN
        );
        let fan_in = effective_merge_fan_in(
            DEFAULT_SORT_CONFIG,
            DEFAULT_SORT_CONFIG.max_record_bytes + NUMERIC_KEY_OVERHEAD,
        );
        assert_eq!(fan_in, 2);
        let max_key_bytes = DEFAULT_SORT_CONFIG.max_record_bytes + NUMERIC_KEY_OVERHEAD;
        let heap_keys = fan_in.saturating_mul(max_key_bytes);
        let pending_key = max_key_bytes;
        let pending_record = DEFAULT_SORT_CONFIG.max_record_bytes;
        let retained_payload = heap_keys
            .saturating_add(pending_key)
            .saturating_add(pending_record);
        assert_eq!(retained_payload, MERGE_MEMORY_BUDGET_BYTES);
    }
}
