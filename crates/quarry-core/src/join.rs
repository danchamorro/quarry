//! Bounded, source-preserving joins with explicit source formats and output layouts.
use crate::source_stamp::open_regular_file;
#[path = "join_working.rs"]
mod working;
pub use working::{
    JoinCellEdits, JoinHeaderEdits, JoinInputOwner, JoinWorkingSource, read_join_working_header,
};

use std::borrow::Cow;
use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use quarry_delimited::parse_record_with_field_limit;

use crate::encoding::RecordScanner;
use crate::export::{ExportTarget, source_matches_stamp};
use crate::spill::{self, FrameHead, FrameLimits, FrameReadError, SpillFile, SpillWorkspace};
use crate::storage::output_parent;
use crate::{Encoding, FilterExportOutcome, QuarryError, SourceStamp, check_storage};

const RECORD_LIMIT: usize = 64 * 1024 * 1024;
const COLUMN_LIMIT: usize = 65_536;
const READ_CHUNK: usize = 64 * 1024;
const MAX_RUN_LEVELS: usize = 64;
const MAX_KEY_PAIRS: usize = 16;
const KEY_FRAME_OVERHEAD: usize = 1 + 8 * MAX_KEY_PAIRS;

/// Interpretation and key selection for one input. Working sources inherit their format.
#[derive(Clone, Debug)]
pub struct JoinInput {
    pub path: PathBuf,
    pub delimiter: Option<u8>,
    pub has_header: bool,
    pub encoding: Option<Encoding>,
    pub key_columns: Vec<usize>,
    /// Bind a UI selection to the file version whose header was shown.
    pub expected_version: Option<JoinSourceVersion>,
    /// Reject repeated nonempty keys when explicitly requested.
    pub require_unique: bool,
}

/// Independent interpretation of a saved source; Auto detects Unicode BOMs only.
#[derive(Clone, Copy, Debug)]
pub struct JoinFormat {
    pub delimiter: Option<u8>,
    pub has_header: bool,
    pub encoding: Option<Encoding>,
}
impl Default for JoinFormat {
    fn default() -> Self {
        Self {
            delimiter: None,
            has_header: true,
            encoding: None,
        }
    }
}

/// Opaque identity of the file whose column labels were presented to the user.
#[derive(Clone, Debug)]
pub struct JoinSourceVersion(SourceStamp);

/// Bounded labels for selecting a saved input's key column.
pub struct JoinHeader {
    pub delimiter: u8,
    pub encoding: Encoding,
    pub has_header: bool,
    pub columns: Vec<String>,
    pub source_version: JoinSourceVersion,
}

/// Check cancellation between bounded filesystem reads and seeks.
struct HeaderReader<'a, R> {
    inner: R,
    state: &'a Progress,
}

impl<R: Read> Read for HeaderReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.state.cancel.load(Ordering::Acquire) {
            return Err(io::Error::other("Header reading cancelled."));
        }
        let limit = buffer.len().min(READ_CHUNK);
        let count = self.inner.read(&mut buffer[..limit])?;
        self.state.bytes.fetch_add(count as u64, Ordering::Release);
        Ok(count)
    }
}

impl<R: Seek> Seek for HeaderReader<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.state.cancel.load(Ordering::Acquire) {
            return Err(io::Error::other("Header reading cancelled."));
        }
        self.inner.seek(position)
    }
}

fn header_read_error(failure: Failure) -> QuarryError {
    match failure {
        Failure::Error(error) => error,
        Failure::Cancelled => error("Header reading cancelled."),
    }
}

fn header_info(path: &Path, format: JoinFormat, state: &Progress) -> JResult<JoinHeader> {
    header_info_using(path, format, None, state)
}

fn header_info_using(
    path: &Path,
    format: JoinFormat,
    working: Option<&JoinWorkingSource>,
    state: &Progress,
) -> JResult<JoinHeader> {
    state.check()?;
    state.phase(JoinPhase::Inspecting);
    if !fs::metadata(path)?.is_file() {
        return Err(input_error(path, None, "Select a regular file.").into());
    }
    let mut file = open_regular_file(path)?;
    if !file.metadata()?.is_file() {
        return Err(input_error(path, None, "Select a regular file.").into());
    }
    let stamp = SourceStamp::from_file(&file)?;
    let encoding =
        Encoding::detect(&mut file, format.encoding).map_err(|e| input_error(path, None, e))?;
    state.check()?;
    let mut reader = HeaderReader {
        inner: &mut file,
        state,
    };
    let result = (|| -> Result<JoinHeader, QuarryError> {
        let delimiter = if let Some(delimiter) = format.delimiter {
            RecordScanner::new(delimiter, encoding)?;
            delimiter
        } else {
            crate::detect_file_delimiter(
                &mut reader,
                stamp.file_size(),
                crate::DEFAULT_SAMPLE_BYTES,
                crate::DEFAULT_BOOTSTRAP_LIMIT,
                encoding,
            )?
        };
        reader.seek(SeekFrom::Start(0))?;
        let (bytes, ends) = crate::read_initial_records(
            &mut reader,
            stamp.file_size(),
            delimiter,
            1,
            crate::DEFAULT_BOOTSTRAP_LIMIT,
            encoding,
        )?;
        let end = *ends.first().ok_or_else(|| {
            input_error(
                path,
                None,
                "The file is empty and has no usable column schema.",
            )
        })? as usize;
        let (_, native) = encoding.strip_bom(&bytes[..end]);
        let decoded = encoding
            .decode(native, RECORD_LIMIT)
            .map_err(|e| input_error(path, Some(1), e))?;
        let effective = match working {
            Some(source) => source
                .record(&decoded, 0, JoinConfig::default(), state)
                .map_err(header_read_error)?,
            None => Cow::Borrowed(decoded.as_ref()),
        };
        let parsed = fields(&effective, delimiter, false, path, 0, JoinConfig::default())
            .map_err(header_read_error)?;
        if parsed.is_empty() {
            return Err(input_error(path, None, "The file has no usable columns."));
        }
        let each = (512 * 1024 / parsed.len()).clamp(1, 256);
        let columns = if format.has_header {
            parsed
                .iter()
                .map(|field| clip_label(std::str::from_utf8(field).expect("validated UTF-8"), each))
                .collect()
        } else {
            vec![String::new(); parsed.len()]
        };
        Ok(JoinHeader {
            delimiter,
            encoding,
            has_header: format.has_header,
            columns,
            source_version: JoinSourceVersion(stamp.clone()),
        })
    })();
    state.check()?;
    let header = result?;
    if !source_matches_stamp(&file, path, &stamp)? {
        return Err(QuarryError::SourceChanged.into());
    }
    Ok(header)
}

/// Read only bounded key-picker metadata with the same cancellable job lifecycle as analysis.
pub fn read_join_header(path: PathBuf) -> Result<JoinJob<JoinHeader>, QuarryError> {
    read_join_header_with_format(path, JoinFormat::default())
}

/// Read bounded picker metadata using explicit source interpretation.
pub fn read_join_header_with_format(
    path: PathBuf,
    format: JoinFormat,
) -> Result<JoinJob<JoinHeader>, QuarryError> {
    JoinJob::spawn(move |state| header_info(&path, format, state))
}

fn clip_label(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn discovered_header(input: &Inspected, config: JoinConfig) -> JoinHeader {
    let budget = config.max_record_bytes.min(512 * 1024);
    let each = (budget / input.info.columns.len().max(1)).min(256);
    JoinHeader {
        delimiter: input.info.delimiter,
        encoding: input.info.encoding,
        has_header: input.info.has_header,
        columns: input
            .info
            .columns
            .iter()
            .map(|s| clip_label(s, each))
            .collect(),
        source_version: JoinSourceVersion(input.stamp.clone()),
    }
}

/// Case comparison affects keys only; stored and emitted field values are unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JoinCase {
    #[default]
    Sensitive,
    AsciiInsensitive,
}

impl JoinCase {
    fn byte(self, value: u8) -> u8 {
        match self {
            Self::Sensitive => value,
            Self::AsciiInsensitive => value.to_ascii_lowercase(),
        }
    }
}

/// Which source rows are retained, with stable order defined by the driving side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JoinMode {
    #[default]
    Left,
    Inner,
    Right,
    Full,
}

impl JoinMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "Keep all Main rows",
            Self::Inner => "Matching rows only",
            Self::Right => "Keep all Additional rows",
            Self::Full => "Keep all rows from both files",
        }
    }

    fn retains(self, main: bool, additional: bool) -> bool {
        match (main, additional) {
            (true, true) => true,
            (true, false) => matches!(self, Self::Left | Self::Full),
            (false, true) => matches!(self, Self::Right | Self::Full),
            (false, false) => false,
        }
    }
}

/// Join behavior and resource limits, shared by analysis and creation.
#[derive(Clone, Copy, Debug)]
pub struct JoinConfig {
    pub mode: JoinMode,
    pub case: JoinCase,
    pub memory_bytes: usize,
    pub run_memory_bytes: usize,
    /// Target capacity of the driving-side row block; a single larger record is allowed.
    pub group_memory_bytes: usize,
    pub max_record_bytes: usize,
    pub max_columns: usize,
    pub max_merge_fan_in: usize,
    pub preview_rows: usize,
    pub preview_bytes: usize,
}

impl Default for JoinConfig {
    fn default() -> Self {
        Self {
            mode: JoinMode::Left,
            case: JoinCase::Sensitive,
            memory_bytes: 512 * 1024 * 1024,
            run_memory_bytes: 16 * 1024 * 1024,
            group_memory_bytes: 8 * 1024 * 1024,
            max_record_bytes: RECORD_LIMIT,
            max_columns: COLUMN_LIMIT,
            max_merge_fan_in: 32,
            preview_rows: 20,
            preview_bytes: 128 * 1024,
        }
    }
}

/// Validated source metadata; full names are retained independently of the grid.
#[derive(Debug)]
pub struct JoinInputInfo {
    pub path: PathBuf,
    pub working_values: bool,
    pub delimiter: u8,
    pub encoding: Encoding,
    pub columns: Vec<String>,
    pub has_header: bool,
    pub data_rows: u64,
    pub empty_key_rows: u64,
    pub bytes: u64,
}

/// Source identity for an output column, independent of its output position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JoinSource {
    Main,
    Additional,
}
impl JoinSource {
    pub fn index(self) -> usize {
        match self {
            Self::Main => 0,
            Self::Additional => 1,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Main => "Main",
            Self::Additional => "Additional",
        }
    }
}

/// One selected source column. None keeps an automatically disambiguated heading.
#[derive(Clone, Debug)]
pub struct JoinOutputColumn {
    pub source: JoinSource,
    pub column: usize,
    pub name: Option<String>,
}

/// Output choices are owned by the immutable analysis, separate from resource policy.
#[derive(Clone, Debug, Default)]
pub struct JoinOutputOptions {
    /// Widths presented before full analysis. Newly discovered fields are included.
    pub known_widths: Option<[usize; 2]>,
    pub columns: Option<Vec<JoinOutputColumn>>,
    pub include_header: Option<bool>,
}

#[derive(Clone, Copy)]
struct ColumnRef {
    source: JoinSource,
    column: usize,
}

struct OutputLayout {
    columns: Vec<ColumnRef>,
    headers: Arc<[String]>,
    include_header: bool,
    duplicate_headers: bool,
    new_columns: [usize; 2],
}

/// Counts describe the entire input; only preview values are sampled and clipped.
#[derive(Debug)]
pub struct JoinAnalysis {
    pub input_rows: [u64; 2],
    pub matching_rows: [u64; 2],
    pub matching_pairs: u64,
    pub duplicate_groups: [u64; 2],
    pub many_to_many_groups: u64,
    pub many_to_many_pairs: u64,
    pub duplicate_samples: Vec<JoinDuplicateExample>,
    pub unmatched_rows: [u64; 2],
    pub empty_key_rows: [u64; 2],
    pub output_rows: u64,
    pub output_bytes: u64,
    pub additional_temporary_bytes: u64,
    pub preview: Vec<Vec<String>>,
    pub output_headers: Arc<[String]>,
    pub include_header: bool,
    pub duplicate_headers: bool,
    /// Columns found beyond the caller's presented widths; require layout review.
    pub new_columns: [usize; 2],
}

/// Bounded examples identify duplicate groups without retaining their records.
#[derive(Debug)]
pub struct JoinDuplicateExample {
    pub key: String,
    pub main_rows: u64,
    pub additional_rows: u64,
}

/// Immutable analysis owns its private sorted inputs until creation or release.
pub struct JoinPlan {
    inputs: [PreparedInput; 2],
    analysis: JoinAnalysis,
    output: OutputLayout,
    workspace: Option<SpillWorkspace>,
    config: JoinConfig,
}

#[derive(Debug)]
pub struct JoinSummary {
    pub destination: PathBuf,
    pub data_rows: u64,
    pub bytes_written: u64,
    pub include_header: bool,
    pub peak_temporary_bytes: u64,
    /// Peak Main-side block capacity, excluding the separately streamed Additional row.
    pub peak_group_buffer_bytes: usize,
    pub cleanup_warning: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinPhase {
    Inspecting,
    Sorting,
    Analyzing,
    Creating,
    Publishing,
}

impl JoinPhase {
    /// Provide a stable user-facing name for each background phase.
    pub fn label(self) -> &'static str {
        match self {
            Self::Inspecting => "Inspecting inputs",
            Self::Sorting => "Sorting keys",
            Self::Analyzing => "Analyzing matches",
            Self::Creating => "Creating joined rows",
            Self::Publishing => "Saving joined file",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct JoinProgress {
    pub phase: JoinPhase,
    pub records: u64,
    pub bytes_scanned: u64,
    pub temporary_bytes: u64,
    pub done: bool,
}

struct Progress {
    phase: AtomicUsize,
    records: AtomicU64,
    bytes: AtomicU64,
    temporary: AtomicU64,
    peak: AtomicU64,
    group_peak: AtomicUsize,
    cancel: AtomicBool,
    done: AtomicBool,
    discovered: Mutex<Option<[JoinHeader; 2]>>,
}

impl Progress {
    fn new() -> Self {
        Self {
            phase: AtomicUsize::new(0),
            records: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            temporary: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            group_peak: AtomicUsize::new(0),
            cancel: AtomicBool::new(false),
            done: AtomicBool::new(false),
            discovered: Mutex::new(None),
        }
    }

    /// Stop at bounded work checkpoints when cancellation is requested.
    fn check(&self) -> JResult<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(Failure::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Change the visible phase and reset its phase-local row counter.
    fn phase(&self, phase: JoinPhase) {
        self.phase.store(phase as usize, Ordering::Release);
        self.records.store(0, Ordering::Release);
        #[cfg(test)]
        crate::join_faults::observe(crate::join_faults::Event::Phase(phase));
    }

    /// Track logical spill and staging bytes with checked arithmetic.
    fn add_disk(&self, bytes: u64) -> JResult<()> {
        let current = self.temporary.load(Ordering::Acquire);
        let next = add(current, bytes)?;
        self.temporary.store(next, Ordering::Release);
        self.peak.fetch_max(next, Ordering::AcqRel);
        Ok(())
    }
}

/// Analysis and output jobs never perform file I/O on the UI thread.
pub struct JoinJob<T> {
    state: Arc<Progress>,
    handle: Option<JoinHandle<Result<Option<T>, QuarryError>>>,
}

impl<T: Send + 'static> JoinJob<T> {
    /// Mark completion even during unwinding so UI polling cannot wait forever.
    fn spawn(
        work: impl FnOnce(&Progress) -> JResult<T> + Send + 'static,
    ) -> Result<Self, QuarryError> {
        #[cfg(test)]
        let instrumentation = crate::join_faults::capture();
        let state = Arc::new(Progress::new());
        let worker = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("quarry-join".into())
            .spawn(move || {
                #[cfg(test)]
                let _instrumentation = crate::join_faults::install(instrumentation);
                struct Done<'a>(&'a AtomicBool);
                impl Drop for Done<'_> {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::Release);
                    }
                }
                let _done = Done(&worker.done);
                match work(&worker) {
                    Ok(value) => Ok(Some(value)),
                    Err(Failure::Cancelled) => Ok(None),
                    Err(Failure::Error(error)) => Err(error),
                }
            })?;
        Ok(Self {
            state,
            handle: Some(handle),
        })
    }

    /// Consume a finished worker, or wait after explicitly requesting cancellation.
    pub fn wait(mut self) -> Result<Option<T>, QuarryError> {
        self.handle
            .take()
            .expect("join worker exists")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl<T> JoinJob<T> {
    /// Take bounded full-scan schema metadata, even if the requested layout failed.
    pub fn take_discovered_headers(&self) -> Option<[JoinHeader; 2]> {
        self.state.discovered.lock().ok()?.take()
    }

    /// Request cooperative cancellation; a published result remains successful.
    pub fn cancel(&self) {
        self.state.cancel.store(true, Ordering::Release);
    }

    /// Read progress without joining or blocking the UI thread.
    pub fn progress(&self) -> JoinProgress {
        let phase = match self.state.phase.load(Ordering::Acquire) {
            0 => JoinPhase::Inspecting,
            1 => JoinPhase::Sorting,
            2 => JoinPhase::Analyzing,
            3 => JoinPhase::Creating,
            _ => JoinPhase::Publishing,
        };
        JoinProgress {
            phase,
            records: self.state.records.load(Ordering::Acquire),
            bytes_scanned: self.state.bytes.load(Ordering::Acquire),
            temporary_bytes: self.state.temporary.load(Ordering::Acquire),
            done: self.state.done.load(Ordering::Acquire),
        }
    }
}

impl<T> Drop for JoinJob<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

#[derive(Debug)]
enum Failure {
    Cancelled,
    Error(QuarryError),
}
type JResult<T> = Result<T, Failure>;
impl Failure {
    fn description(&self) -> String {
        match self {
            Self::Cancelled => "Join cancelled".into(),
            Self::Error(error) => error.to_string(),
        }
    }
}

impl From<QuarryError> for Failure {
    fn from(error: QuarryError) -> Self {
        Self::Error(error)
    }
}
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Error(error.into())
    }
}

/// Report an operation-wide failure without inventing a source location.
fn error(reason: impl ToString) -> QuarryError {
    QuarryError::JoinFile {
        path: None,
        record: None,
        reason: reason.to_string(),
    }
}
/// Retain the input path and physical record in user-facing failures.
fn input_error(path: &Path, record: Option<u64>, reason: impl ToString) -> QuarryError {
    QuarryError::JoinFile {
        path: Some(path.to_path_buf()),
        record,
        reason: reason.to_string(),
    }
}
/// Reject cardinality or byte-count overflow instead of wrapping.
fn add(left: u64, right: u64) -> JResult<u64> {
    left.checked_add(right)
        .ok_or_else(|| error("Join size or row count exceeds the supported range.").into())
}
/// Reject expansion or allocation overflow before any output is created.
fn mul(left: u64, right: u64) -> JResult<u64> {
    left.checked_mul(right)
        .ok_or_else(|| error("Join size or row count exceeds the supported range.").into())
}

impl JoinConfig {
    /// Require the configured live payloads and bookkeeping to fit the memory ceiling.
    fn validate(self, temporary: &Path) -> Result<(), QuarryError> {
        if self.memory_bytes > 512 * 1024 * 1024
            || self.max_record_bytes == 0
            || self.max_record_bytes > RECORD_LIMIT
            || self.max_columns == 0
            || self.max_columns > COLUMN_LIMIT
            || self.run_memory_bytes == 0
            || self.group_memory_bytes == 0
            || self.max_merge_fan_in < 2
            || self.max_merge_fan_in > 32
            || self.preview_rows > 100
            || self.preview_bytes > 1024 * 1024
        {
            return Err(error("Invalid join resource limits."));
        }
        // Worst concurrent payloads: two headers, two records, two parsed
        // quoted-field copies, and one output. Run buffers, previews, column
        // metadata, bounded run paths and reader buffers are accounted separately.
        let reserved = self.reserved_bytes(temporary);
        if reserved > self.memory_bytes {
            return Err(error(
                "The join memory budget cannot fit its record, preview and merge limits.",
            ));
        }
        Ok(())
    }

    fn reserved_bytes(self, temporary: &Path) -> usize {
        // Include directory ownership, live file identities and tree-node overhead.
        let path_bytes = temporary
            .as_os_str()
            .len()
            .saturating_mul(2)
            .saturating_add(512);
        self.max_record_bytes
            .saturating_mul(7)
            .saturating_add(KEY_FRAME_OVERHEAD * 7)
            .saturating_add(self.run_memory_bytes)
            .saturating_add(self.group_memory_bytes)
            .saturating_add(self.preview_bytes)
            .saturating_add(self.max_record_bytes.min(512 * 1024).saturating_mul(2))
            .saturating_add(self.max_columns.saturating_mul(384))
            .saturating_add(
                MAX_RUN_LEVELS
                    .saturating_mul(self.max_merge_fan_in)
                    .saturating_mul(path_bytes),
            )
            .saturating_add(READ_CHUNK.saturating_mul(self.max_merge_fan_in + 1))
    }

    /// Derive a bounded merge width from observed keys and reserved live memory.
    fn fan_in(self, max_key: usize) -> usize {
        // Headers and one record remain live while merging. Keep an additional
        // head allowance for advancing a stream, plus fixed I/O and path state.
        let reserved = self
            .max_record_bytes
            .saturating_mul(3)
            .saturating_add(self.run_memory_bytes)
            .saturating_add(self.group_memory_bytes)
            .saturating_add(self.preview_bytes)
            .saturating_add(self.max_columns.saturating_mul(64))
            .saturating_add(4 * 1024 * 1024);
        let head = max_key.saturating_add(8192 + 64).max(1);
        self.max_merge_fan_in
            .min(self.memory_bytes.saturating_sub(reserved) / head)
            .max(2)
    }

    /// Keep every private frame within the input record and key limits.
    fn frame_limits(self) -> FrameLimits {
        FrameLimits {
            max_key_bytes: self
                .max_record_bytes
                .saturating_add(KEY_FRAME_OVERHEAD)
                .max(16),
            max_record_bytes: self.max_record_bytes,
        }
    }
}

#[derive(Debug)]
struct PreparedInput {
    info: JoinInputInfo,
    physical_path: PathBuf,
    working: Option<JoinWorkingSource>,
    stamp: SourceStamp,
    key_columns: Vec<usize>,
    require_unique: bool,
    output_delimiter: u8,
    projection: Vec<(usize, bool)>,
    sorted: SpillFile,
    frame_bytes: u64,
}

struct Entry {
    key: Vec<u8>,
    record: Vec<u8>,
    ordinal: u64,
}
struct Run {
    path: SpillFile,
    bytes: u64,
}

struct HeapEntry {
    head: FrameHead,
    stream: usize,
}
impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        (self.head.key.as_slice(), self.head.ordinal, self.stream)
            == (other.head.key.as_slice(), other.head.ordinal, other.stream)
    }
}
impl Eq for HeapEntry {}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        (other.head.key.as_slice(), other.head.ordinal, other.stream).cmp(&(
            self.head.key.as_slice(),
            self.head.ordinal,
            self.stream,
        ))
    }
}

/// Cap every physical read so cancellation is observed inside a large frame.
struct CancelRead<'a, R> {
    reader: &'a mut R,
    state: &'a Progress,
}
impl<R: Read> Read for CancelRead<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.state.cancel.load(Ordering::Acquire) {
            return Err(io::Error::other("join cancelled"));
        }
        let length = bytes.len().min(READ_CHUNK);
        self.reader.read(&mut bytes[..length])
    }
}

/// Preserve I/O failures while translating cancellation and malformed frames.
fn frame_error(failure: FrameReadError, state: &Progress) -> Failure {
    if state.cancel.load(Ordering::Acquire) {
        return Failure::Cancelled;
    }
    match failure {
        FrameReadError::Io(cause) => cause.into(),
        FrameReadError::KeyLength => error("Invalid private join key length.").into(),
        FrameReadError::RecordLength => error("Invalid private join record length.").into(),
    }
}

/// Read one bounded key while retaining its payload on disk.
fn read_head(
    reader: &mut impl Read,
    config: JoinConfig,
    state: &Progress,
) -> JResult<Option<FrameHead>> {
    spill::read_head(&mut CancelRead { reader, state }, config.frame_limits())
        .map_err(|error| frame_error(error, state))
}
/// Read a bounded payload with cancellation checkpoints inside large records.
fn read_record(
    reader: &mut impl Read,
    length: usize,
    config: JoinConfig,
    state: &Progress,
) -> JResult<Vec<u8>> {
    spill::read_record(
        &mut CancelRead { reader, state },
        length,
        config.max_record_bytes,
    )
    .map_err(|error| frame_error(error, state))
}

/// Leveled compaction caps the number of retained run paths independently of
/// source length, instead of collecting one path per small initial run forever.
struct Runs<'a> {
    workspace: &'a SpillWorkspace,
    config: JoinConfig,
    state: &'a Progress,
    fan_in: usize,
    levels: Vec<Vec<Run>>,
    buffer: Vec<Entry>,
    buffer_bytes: usize,
}

impl<'a> Runs<'a> {
    fn new(
        workspace: &'a SpillWorkspace,
        config: JoinConfig,
        state: &'a Progress,
        max_key: usize,
    ) -> Self {
        Self {
            workspace,
            config,
            state,
            fan_in: config.fan_in(max_key),
            levels: vec![],
            buffer: vec![],
            buffer_bytes: 0,
        }
    }

    fn spill_io_error(&self, error: io::Error) -> QuarryError {
        let phase = if self.state.phase.load(Ordering::Acquire) == JoinPhase::Creating as usize {
            JoinPhase::Creating
        } else {
            JoinPhase::Sorting
        };
        crate::storage::storage_error(
            &self.workspace.path,
            io::Error::new(
                error.kind(),
                format!(
                    "{}: {error}. Free disk space or choose another temporary folder, then analyze again.",
                    phase.label()
                ),
            ),
        )
    }

    fn write_frame(
        &self,
        writer: &mut impl Write,
        key: &[u8],
        record: &[u8],
        ordinal: u64,
    ) -> JResult<u64> {
        #[cfg(test)]
        crate::join_faults::point(crate::join_faults::Event::SpillWrite)
            .map_err(|error| self.spill_io_error(error))?;
        spill::write_frame(writer, key, record, ordinal)
            .map_err(|error| self.spill_io_error(error).into())
    }

    fn flush_writer(&self, writer: &mut impl Write) -> JResult<()> {
        #[cfg(test)]
        crate::join_faults::point(crate::join_faults::Event::SpillFlush)
            .map_err(|error| self.spill_io_error(error))?;
        writer
            .flush()
            .map_err(|error| self.spill_io_error(error).into())
    }

    /// Flush accumulated rows before admitting another record beyond the run target.
    fn push(&mut self, entry: Entry) -> JResult<()> {
        self.state.check()?;
        let size = entry
            .key
            .capacity()
            .saturating_add(entry.record.capacity())
            .saturating_add(std::mem::size_of::<Entry>());
        if !self.buffer.is_empty()
            && self.buffer_bytes.saturating_add(size) > self.config.run_memory_bytes
        {
            self.flush()?;
        }
        self.buffer_bytes = self.buffer_bytes.saturating_add(size);
        self.buffer.push(entry);
        Ok(())
    }

    /// Write a sorted private run, breaking equal keys by source ordinal.
    fn flush(&mut self) -> JResult<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.state.check()?;
        let mut entries = std::mem::take(&mut self.buffer);
        self.buffer_bytes = 0;
        entries.sort_unstable_by(|a, b| a.key.cmp(&b.key).then_with(|| a.ordinal.cmp(&b.ordinal)));
        let (path, mut writer) = self.workspace.create_run()?;
        let mut bytes = 0;
        for entry in entries {
            self.state.check()?;
            let count = self.write_frame(&mut writer, &entry.key, &entry.record, entry.ordinal)?;
            bytes = add(bytes, count)?;
            self.state.add_disk(count)?;
        }
        self.flush_writer(&mut writer)?;
        drop(writer);
        self.carry(Run { path, bytes }, 0)
    }

    /// Compact full levels so run-path bookkeeping stays bounded.
    fn carry(&mut self, run: Run, mut level: usize) -> JResult<()> {
        let mut pending = run;
        loop {
            if level >= MAX_RUN_LEVELS {
                return Err(error("Too many join spill generations.").into());
            }
            if self.levels.len() <= level {
                self.levels.push(vec![]);
            }
            self.levels[level].push(pending);
            if self.levels[level].len() < self.fan_in {
                return Ok(());
            }
            let group = std::mem::take(&mut self.levels[level]);
            #[cfg(test)]
            crate::join_faults::observe(crate::join_faults::Event::Merge);
            pending = self.merge(group)?;
            level += 1;
        }
    }

    /// Keep one key per input stream and one payload live during a merge.
    fn merge(&self, runs: Vec<Run>) -> JResult<Run> {
        self.state.check()?;
        let mut readers: Vec<_> = runs
            .iter()
            .map(|run| run.path.open().map(BufReader::new))
            .collect::<Result<_, _>>()?;
        let mut heap = BinaryHeap::new();
        for (stream, reader) in readers.iter_mut().enumerate() {
            if let Some(head) = read_head(reader, self.config, self.state)? {
                heap.push(HeapEntry { head, stream });
            }
        }
        let (path, mut writer) = self.workspace.create_run()?;
        let mut bytes = 0;
        while let Some(entry) = heap.pop() {
            let record = read_record(
                &mut readers[entry.stream],
                entry.head.record_len,
                self.config,
                self.state,
            )?;
            let count =
                self.write_frame(&mut writer, &entry.head.key, &record, entry.head.ordinal)?;
            bytes = add(bytes, count)?;
            self.state.add_disk(count)?;
            let stream = entry.stream;
            drop(record);
            drop(entry);
            if let Some(head) = read_head(&mut readers[stream], self.config, self.state)? {
                heap.push(HeapEntry { head, stream });
            }
        }
        self.flush_writer(&mut writer)?;
        drop(writer);
        drop(readers);
        for run in runs {
            run.path.remove()?;
            self.state.temporary.fetch_sub(run.bytes, Ordering::AcqRel);
        }
        Ok(Run { path, bytes })
    }

    /// Collapse all retained levels into a single ordered private stream.
    fn finish(mut self) -> JResult<Run> {
        self.flush()?;
        let mut runs: Vec<_> = self.levels.drain(..).flatten().collect();
        if runs.is_empty() {
            let (path, writer) = self.workspace.create_run()?;
            drop(writer);
            return Ok(Run { path, bytes: 0 });
        }
        while runs.len() > 1 {
            let mut next = vec![];
            let mut pending = runs.into_iter();
            loop {
                let group: Vec<_> = pending.by_ref().take(self.fan_in).collect();
                if group.is_empty() {
                    break;
                }
                next.push(if group.len() == 1 {
                    group.into_iter().next().unwrap()
                } else {
                    self.merge(group)?
                });
            }
            runs = next;
        }
        Ok(runs.pop().unwrap())
    }
}

fn check_source(input: &PreparedInput) -> JResult<File> {
    if let Some(working) = &input.working {
        working.validate()?;
    }
    let file = open_regular_file(&input.physical_path)
        .map_err(|e| input_error(&input.info.path, None, e))?;
    if !source_matches_stamp(&file, &input.physical_path, &input.stamp)? {
        return Err(input_error(
            &input.info.path,
            None,
            "File changed. Analyze the inputs again.",
        )
        .into());
    }
    Ok(file)
}

/// Scan complete records without retaining the file or assuming the first chunk
/// describes its width, text validity, or number of rows.
struct ScanSource<'a> {
    path: &'a Path,
    stamp: &'a SourceStamp,
    working: Option<&'a JoinWorkingSource>,
}

fn scan(
    source: ScanSource<'_>,
    delimiter: u8,
    encoding: Encoding,
    config: JoinConfig,
    state: &Progress,
    mut visit: impl FnMut(&[u8], u64) -> JResult<()>,
) -> JResult<()> {
    let path = source.path;
    let stamp = source.stamp;
    let display_path = source.working.map_or(path, JoinWorkingSource::logical_path);
    if let Some(working) = source.working {
        working.validate()?;
    }
    let mut file = open_regular_file(path).map_err(|e| input_error(display_path, None, e))?;
    if !source_matches_stamp(&file, path, stamp)? {
        return Err(QuarryError::SourceChanged.into());
    }
    // Frame UTF-16 structurally, then decode each complete record for an exact
    // record diagnostic rather than validating an entire read before callbacks.
    let mut scanner = RecordScanner::new_for_inspection(delimiter, encoding)?;
    let chunk_size = if encoding.is_utf16() {
        READ_CHUNK.min(config.max_record_bytes).max(2) / 2 * 2
    } else {
        READ_CHUNK.min(config.max_record_bytes)
    };
    let mut chunk = vec![0; chunk_size];
    let mut record = Vec::new();
    let mut offset = 0_u64;
    let mut row = 0_u64;
    let mut decoded_visit = |record: &[u8], row: u64| -> JResult<()> {
        let native = if row == 0 {
            encoding.strip_bom(record).1
        } else {
            record
        };
        let decoded = encoding
            .decode(native, config.max_record_bytes)
            .map_err(|e| input_error(display_path, Some(row + 1), e))?;
        let effective = match source.working {
            Some(working) => working.record(&decoded, row, config, state)?,
            None => Cow::Borrowed(decoded.as_ref()),
        };
        visit(&effective, row)
    };
    loop {
        state.check()?;
        let mut native_read = 0;
        while native_read < chunk.len() {
            state.check()?;
            let count = file.read(&mut chunk[native_read..])?;
            if count == 0 {
                break;
            }
            native_read += count;
        }
        let incomplete_unit = encoding.is_utf16() && native_read % 2 != 0;
        let read = native_read - usize::from(incomplete_unit);
        if native_read == 0 {
            let mut last = false;
            scanner
                .finish(offset, |_| last = true)
                .map_err(|e| input_error(display_path, Some(row + 1), e))?;
            if last {
                decoded_visit(&record, row)?;
            }
            if !source_matches_stamp(&file, path, stamp)? {
                return Err(QuarryError::SourceChanged.into());
            }
            if let Some(working) = source.working {
                working.validate()?;
                working.check_rows(row + u64::from(last))?;
            }
            return Ok(());
        }
        if add(offset, native_read as u64)? > stamp.file_size() {
            return Err(QuarryError::SourceChanged.into());
        }
        let mut start = 0;
        let mut failed = None;
        let scanned = scanner.scan_chunk(&chunk[..read], offset, |end| {
            if failed.is_some() || state.cancel.load(Ordering::Acquire) {
                return;
            }
            let end = (end - offset) as usize;
            if record.len().saturating_add(end - start) > config.max_record_bytes {
                failed = Some(
                    input_error(
                        display_path,
                        Some(row + 1),
                        "Record exceeds the configured size limit.",
                    )
                    .into(),
                );
                return;
            }
            record.extend_from_slice(&chunk[start..end]);
            if let Err(error) = decoded_visit(&record, row) {
                failed = Some(error);
            }
            record.clear();
            row += 1;
            start = end;
        });
        if let Some(error) = failed {
            return Err(error);
        }
        state.check()?;
        scanned.map_err(|e| input_error(display_path, Some(row + 1), e))?;
        if incomplete_unit {
            return Err(input_error(
                display_path,
                Some(row + 1),
                "Incomplete UTF-16 code unit. Choose the correct encoding; no text was replaced.",
            )
            .into());
        }
        if record.len().saturating_add(read - start) > config.max_record_bytes {
            return Err(input_error(
                display_path,
                Some(row + 1),
                "Record exceeds the configured size limit.",
            )
            .into());
        }
        record.extend_from_slice(&chunk[start..read]);
        offset = add(offset, read as u64)?;
        state.bytes.fetch_add(read as u64, Ordering::Release);
    }
}

/// Validate all text, including unselected columns, before parsing field values.
fn fields<'a>(
    record: &'a [u8],
    delimiter: u8,
    first: bool,
    path: &Path,
    row: u64,
    config: JoinConfig,
) -> JResult<Vec<Cow<'a, [u8]>>> {
    let record = if first {
        record.strip_prefix(b"\xef\xbb\xbf").unwrap_or(record)
    } else {
        record
    };
    std::str::from_utf8(record).map_err(|_| {
        input_error(
            path,
            Some(row + 1),
            "Invalid UTF-8 text. Choose the correct encoding before joining.",
        )
    })?;
    parse_record_with_field_limit(record, delimiter, config.max_columns)
        .map_err(|e| input_error(path, Some(row + 1), e).into())
}

/// Length-prefix every nonempty component so tuple boundaries cannot collide.
fn key(
    fields: &[Cow<'_, [u8]>],
    columns: &[usize],
    ordinal: u64,
    config: JoinConfig,
) -> JResult<Vec<u8>> {
    if columns
        .iter()
        .any(|&column| fields.get(column).is_none_or(|field| field.is_empty()))
    {
        let mut result = vec![0];
        result.extend_from_slice(&ordinal.to_be_bytes());
        return Ok(result);
    }
    let size = columns.iter().try_fold(1_usize, |size, &column| {
        size.checked_add(8)
            .and_then(|n| n.checked_add(fields[column].len()))
            .ok_or_else(|| error("Composite key length overflow."))
    })?;
    if size > config.frame_limits().max_key_bytes {
        return Err(error("A composite key exceeds the comparison-key size limit.").into());
    }
    let mut result = Vec::new();
    result
        .try_reserve_exact(size)
        .map_err(|_| error("Cannot allocate a bounded comparison key."))?;
    result.push(1);
    for &column in columns {
        let value = &fields[column];
        result.extend_from_slice(&(value.len() as u64).to_be_bytes());
        result.extend(value.iter().map(|&byte| config.case.byte(byte)));
    }
    Ok(result)
}

/// Check stored framing without allocating another record-sized comparison key.
fn key_matches(
    stored: &[u8],
    fields: &[Cow<'_, [u8]>],
    columns: &[usize],
    ordinal: u64,
    case: JoinCase,
) -> bool {
    if columns
        .iter()
        .any(|&column| fields.get(column).is_none_or(|field| field.is_empty()))
    {
        return stored.first() == Some(&0)
            && stored.get(1..) == Some(ordinal.to_be_bytes().as_slice());
    }
    if stored.first() != Some(&1) {
        return false;
    }
    let mut rest = &stored[1..];
    for &column in columns {
        let value = &fields[column];
        let Some(length) = rest.get(..8) else {
            return false;
        };
        if u64::from_be_bytes(length.try_into().unwrap()) != value.len() as u64 {
            return false;
        }
        rest = &rest[8..];
        let Some(component) = rest.get(..value.len()) else {
            return false;
        };
        if !component
            .iter()
            .copied()
            .eq(value.iter().map(|&byte| case.byte(byte)))
        {
            return false;
        }
        rest = &rest[value.len()..];
    }
    rest.is_empty()
}

/// Format bounded, unambiguous samples, escaping invisible and delimiter characters.
fn key_label(key: &[u8]) -> JResult<String> {
    if key.first() != Some(&1) {
        return Err(error("Invalid private comparison key.").into());
    }
    let mut rest = &key[1..];
    let mut label = String::new();
    let mut components = 0;
    let mut clipped = false;
    let append = |label: &mut String, text: &str, clipped: &mut bool| {
        if label.len() + text.len() <= 128 {
            label.push_str(text);
        } else {
            *clipped = true;
        }
    };
    append(&mut label, "(", &mut clipped);
    while !rest.is_empty() {
        components += 1;
        if components > MAX_KEY_PAIRS {
            return Err(error("Invalid private key component count.").into());
        }
        let length = rest
            .get(..8)
            .ok_or_else(|| error("Truncated private comparison key."))?;
        let size = usize::try_from(u64::from_be_bytes(length.try_into().unwrap()))
            .map_err(|_| error("Invalid private key component length."))?;
        rest = &rest[8..];
        let value = rest
            .get(..size)
            .ok_or_else(|| error("Truncated private key component."))?;
        if value.is_empty() {
            return Err(error("Empty private key component.").into());
        }
        let text = std::str::from_utf8(value).map_err(|_| error("Invalid private key text."))?;
        if components > 1 {
            append(&mut label, ", ", &mut clipped);
        }
        append(&mut label, "\"", &mut clipped);
        for ch in text.chars() {
            if clipped {
                break;
            }
            match ch {
                '\0' => append(&mut label, "\\0", &mut clipped),
                '\n' => append(&mut label, "\\n", &mut clipped),
                '\r' => append(&mut label, "\\r", &mut clipped),
                '\t' => append(&mut label, "\\t", &mut clipped),
                '"' => append(&mut label, "\\\"", &mut clipped),
                '\\' => append(&mut label, "\\\\", &mut clipped),
                _ if ch.is_control() => append(&mut label, "?", &mut clipped),
                _ => {
                    let mut encoded = [0; 4];
                    append(&mut label, ch.encode_utf8(&mut encoded), &mut clipped);
                }
            }
        }
        append(&mut label, "\"", &mut clipped);
        rest = &rest[size..];
    }
    if components == 0 {
        return Err(error("Missing private key components.").into());
    }
    if clipped {
        label.push('…');
    }
    label.push(')');
    Ok(label)
}

struct Inspected {
    info: JoinInputInfo,
    physical_path: PathBuf,
    working: Option<JoinWorkingSource>,
    stamp: SourceStamp,
    key_columns: Vec<usize>,
    require_unique: bool,
    output_delimiter: u8,
    frame_bytes: u64,
    max_key_bytes: usize,
}

/// Scan the complete source to validate text and size its private frames.
fn inspect(
    input: JoinInput,
    mut working: Option<JoinWorkingSource>,
    config: JoinConfig,
    state: &Progress,
) -> JResult<Inspected> {
    state.phase(JoinPhase::Inspecting);
    if let Some(source) = &mut working {
        source.accepts(&input)?;
        source.pin_private_directory()?;
    }
    let path = if input.path.is_absolute() {
        input.path
    } else {
        std::env::current_dir()?.join(input.path)
    };
    let path = if let Some(source) = &working {
        let logical = source.logical_path();
        if logical.is_absolute() {
            logical.to_path_buf()
        } else {
            std::env::current_dir()?.join(logical)
        }
    } else {
        path
    };
    let physical_path = working
        .as_ref()
        .map_or_else(|| path.clone(), |source| source.session.path.clone());
    let mut file = open_regular_file(&physical_path).map_err(|e| input_error(&path, None, e))?;
    if !file.metadata()?.is_file() {
        return Err(input_error(&path, None, "Select a regular file.").into());
    }
    let stamp = SourceStamp::from_file(&file)?;
    if input
        .expected_version
        .as_ref()
        .is_some_and(|expected| !expected.0.matches(&stamp))
    {
        return Err(input_error(&path, None,
            "The selected file changed after its columns were loaded. Choose that file again before analyzing.").into());
    }

    let encoding = Encoding::detect(
        &mut file,
        working
            .as_ref()
            .map_or(input.encoding, |source| source.format().encoding),
    )
    .map_err(|e| input_error(&path, None, e))?;
    let delimiter = match working
        .as_ref()
        .map_or(input.delimiter, |source| source.format().delimiter)
    {
        Some(value) => {
            RecordScanner::new(value, encoding).map_err(|e| input_error(&path, None, e))?;
            value
        }
        None => crate::detect_file_delimiter(
            &mut file,
            stamp.file_size(),
            crate::DEFAULT_SAMPLE_BYTES,
            crate::DEFAULT_BOOTSTRAP_LIMIT,
            encoding,
        )
        .map_err(|e| input_error(&path, None, e))?,
    };
    drop(file);
    let mut columns = None;
    let mut data_rows = 0;
    let mut empty_key_rows = 0;
    let mut frame_bytes = 0;
    let mut max_key_bytes = 0;
    scan(
        ScanSource {
            path: &physical_path,
            stamp: &stamp,
            working: working.as_ref(),
        },
        delimiter,
        encoding,
        config,
        state,
        |record, row| {
            let parsed = fields(record, delimiter, false, &path, row, config)?;
            if row == 0 {
                columns = Some(if input.has_header {
                    parsed
                        .iter()
                        .map(|field| String::from_utf8(field.to_vec()).expect("validated UTF-8"))
                        .collect::<Vec<_>>()
                } else {
                    vec![String::new(); parsed.len()]
                });
            }
            if input.has_header && row == 0 {
                return Ok(());
            }
            let names = columns.as_mut().expect("first record creates schema");
            if parsed.len() > names.len() {
                names.resize(parsed.len(), String::new());
            }
            let key =
                key(&parsed, &input.key_columns, data_rows, config).map_err(
                    |failure| match failure {
                        Failure::Error(error) => {
                            Failure::Error(input_error(&path, Some(row + 1), error))
                        }
                        cancelled => cancelled,
                    },
                )?;
            if key[0] == 0 {
                empty_key_rows = add(empty_key_rows, 1)?;
            }
            max_key_bytes = max_key_bytes.max(key.len());
            frame_bytes = add(
                frame_bytes,
                add(24, add(key.len() as u64, record.len() as u64)?)?,
            )?;
            data_rows = add(data_rows, 1)?;
            state.records.store(data_rows, Ordering::Release);
            Ok(())
        },
    )?;
    let columns = columns.ok_or_else(|| {
        input_error(
            &path,
            None,
            "The file is empty and has no usable column schema.",
        )
    })?;
    if input
        .key_columns
        .iter()
        .any(|&column| column >= columns.len())
    {
        return Err(input_error(
            &path,
            None,
            "A selected key column is outside the complete source schema.",
        )
        .into());
    }
    Ok(Inspected {
        info: JoinInputInfo {
            path,
            working_values: working.is_some(),
            delimiter,
            encoding,
            columns,
            has_header: input.has_header,
            data_rows,
            empty_key_rows,
            bytes: stamp.file_size(),
        },
        physical_path,
        working,
        stamp,
        key_columns: input.key_columns,
        require_unique: input.require_unique,
        output_delimiter: delimiter,
        frame_bytes,
        max_key_bytes,
    })
}

/// Reject canonical aliases and Unix hard links as self joins.
fn same_input(left: &Inspected, right: &Inspected) -> JResult<bool> {
    let left_path = left
        .working
        .as_ref()
        .map_or(left.info.path.as_path(), JoinWorkingSource::logical_path);
    let right_path = right
        .working
        .as_ref()
        .map_or(right.info.path.as_path(), JoinWorkingSource::logical_path);
    if left_path.canonicalize()? == right_path.canonicalize()? {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = fs::metadata(left_path)?;
        let b = fs::metadata(right_path)?;
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    #[cfg(not(unix))]
    {
        // Conservatively refuse indistinguishable saved identities on platforms
        // without the Unix inode guard; never risk a same-source join.
        Ok(left
            .working
            .as_ref()
            .map_or(&left.stamp, JoinWorkingSource::logical_stamp)
            .matches(
                right
                    .working
                    .as_ref()
                    .map_or(&right.stamp, JoinWorkingSource::logical_stamp),
            ))
    }
}

/// Recheck the inspected source while producing its ordered private stream.
fn prepare(
    input: Inspected,
    workspace: &SpillWorkspace,
    config: JoinConfig,
    state: &Progress,
) -> JResult<PreparedInput> {
    state.phase(JoinPhase::Sorting);
    let mut runs = Runs::new(workspace, config, state, input.max_key_bytes);
    let mut rows = 0;
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
        |record, row| {
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
            let comparison = key(&parsed, &input.key_columns, rows, config)?;
            drop(parsed);
            runs.push(Entry {
                key: comparison,
                record: record.to_vec(),
                ordinal: rows,
            })?;
            rows = add(rows, 1)?;
            Ok(())
        },
    )?;
    if rows != input.info.data_rows {
        return Err(QuarryError::SourceChanged.into());
    }
    let run = runs.finish()?;
    if run.bytes != input.frame_bytes {
        return Err(error("Prepared join data differs from the inspected input.").into());
    }
    Ok(PreparedInput {
        info: input.info,
        physical_path: input.physical_path,
        working: input.working,
        stamp: input.stamp,
        key_columns: input.key_columns,
        require_unique: input.require_unique,
        output_delimiter: input.output_delimiter,
        projection: Vec::new(),
        sorted: run.path,
        frame_bytes: run.bytes,
    })
}

/// Serialize decoded fields with exact value preservation and a hard record cap.
fn encode<'a>(
    fields: impl IntoIterator<Item = &'a [u8]>,
    delimiter: u8,
    limit: usize,
) -> JResult<Vec<u8>> {
    let mut output = Vec::new();
    for (column, field) in fields.into_iter().enumerate() {
        let quoted = field
            .iter()
            .any(|byte| [delimiter, b'"', b'\r', b'\n'].contains(byte))
            || (column == 0 && field.starts_with(b"\xef\xbb\xbf"));
        let escaped = if quoted {
            field.iter().filter(|byte| **byte == b'"').count() + 2
        } else {
            0
        };
        let extra = field
            .len()
            .checked_add(escaped)
            .and_then(|n| n.checked_add(usize::from(column > 0)))
            .ok_or_else(|| error("Joined record length overflow."))?;
        if output.len().saturating_add(extra).saturating_add(1) > limit {
            return Err(
                error("A joined record or header exceeds the configured size limit.").into(),
            );
        }
        if column > 0 {
            output.push(delimiter);
        }
        if quoted {
            output.push(b'"');
        }
        for &byte in field {
            output.push(byte);
            if quoted && byte == b'"' {
                output.push(byte);
            }
        }
        if quoted {
            output.push(b'"');
        }
    }
    output.push(b'\n');
    Ok(output)
}

impl OutputLayout {
    fn resolve(
        inputs: [&JoinInputInfo; 2],
        options: JoinOutputOptions,
        config: JoinConfig,
        temporary: &Path,
        state: &Progress,
    ) -> JResult<Self> {
        use std::collections::{HashMap, HashSet};
        let mut requested = if let Some(columns) = options.columns {
            columns
        } else {
            let count = inputs[0]
                .columns
                .len()
                .checked_add(inputs[1].columns.len())
                .ok_or_else(|| error("Too many result columns."))?;
            if count > config.max_columns {
                return Err(error("The result has too many columns. Select fewer columns.").into());
            }
            [JoinSource::Main, JoinSource::Additional]
                .into_iter()
                .flat_map(|source| {
                    (0..inputs[source.index()].columns.len()).map(move |column| JoinOutputColumn {
                        source,
                        column,
                        name: None,
                    })
                })
                .collect()
        };
        let new_columns = options.known_widths.map_or([0, 0], |known| {
            std::array::from_fn(|side| inputs[side].columns.len().saturating_sub(known[side]))
        });
        if let Some(known) = options.known_widths {
            let mut included: std::collections::HashSet<_> = requested
                .iter()
                .map(|column| (column.source, column.column))
                .collect();
            for source in [JoinSource::Main, JoinSource::Additional] {
                for column in known[source.index()]..inputs[source.index()].columns.len() {
                    state.check()?;
                    if included.insert((source, column)) {
                        if requested.len() >= config.max_columns {
                            return Err(error("Newly discovered columns exceed the output column limit. Review the complete source columns and select fewer fields.").into());
                        }
                        requested.push(JoinOutputColumn {
                            source,
                            column,
                            name: None,
                        });
                    }
                }
            }
        }
        if requested.is_empty() || requested.len() > config.max_columns {
            return Err(error(
                "Select between one and the supported maximum number of output columns.",
            )
            .into());
        }
        let include_header = options
            .include_header
            .unwrap_or(inputs.iter().any(|input| input.has_header));
        let mut counts = HashMap::new();
        for column in &requested {
            let original = inputs[column.source.index()]
                .columns
                .get(column.column)
                .ok_or_else(|| {
                    error(format!(
                        "{} output column {} is outside the source schema.",
                        column.source.label(),
                        column.column + 1
                    ))
                })?;
            *counts
                .entry(column.name.as_deref().unwrap_or(original))
                .or_insert(0_usize) += 1;
        }
        // Reserve transient requested names, generated names and collision maps,
        // as well as the headings retained by the immutable plan. Column metadata
        // has its own worst-case reserve in JoinConfig.
        let name_budget = config
            .memory_bytes
            .saturating_sub(config.reserved_bytes(temporary))
            .saturating_div(4)
            .min(config.max_record_bytes);
        let requested_name_bytes = requested
            .iter()
            .filter_map(|column| column.name.as_ref())
            .fold(0_usize, |total, name| {
                total.saturating_add(name.len()).saturating_add(1)
            });
        if requested_name_bytes > name_budget {
            return Err(error(
                "The requested headings exceed the join memory or record-size limit.",
            )
            .into());
        }
        let mut name_bytes = 0_usize;
        let mut used: HashSet<String> = requested.iter().filter_map(|c| c.name.clone()).collect();
        let mut suffixes: HashMap<String, usize> = HashMap::new();
        let mut headers = Vec::with_capacity(requested.len());
        let mut columns = Vec::with_capacity(requested.len());
        for column in &requested {
            state.check()?;
            let role = column.source.label();
            let original = &inputs[column.source.index()].columns[column.column];
            let name = if let Some(name) = &column.name {
                name.clone()
            } else {
                let base = if original.is_empty() {
                    format!("{role} column {}", column.column + 1)
                } else if counts[original.as_str()] > 1 {
                    format!("{original} ({role})")
                } else {
                    original.clone()
                };
                let mut candidate = base.clone();
                if used.contains(&candidate) {
                    let next = suffixes.entry(base.clone()).or_insert(2);
                    loop {
                        state.check()?;
                        candidate = format!("{base} {next}");
                        *next += 1;
                        if !used.contains(&candidate) {
                            break;
                        }
                    }
                }
                used.insert(candidate.clone());
                candidate
            };
            name_bytes = name_bytes.saturating_add(name.len()).saturating_add(1);
            if name_bytes > name_budget {
                return Err(error("The result column names exceed the join memory or record-size limit. Select fewer columns or use shorter headings.").into());
            }
            headers.push(name);
            columns.push(ColumnRef {
                source: column.source,
                column: column.column,
            });
        }
        let mut unique = HashSet::new();
        let duplicate_headers = headers.iter().any(|name| !unique.insert(name.as_str()));
        let layout = Self {
            columns,
            headers: headers.into(),
            include_header,
            duplicate_headers,
            new_columns,
        };
        if include_header {
            output_header(&layout, inputs[0].delimiter, config)?;
        }
        Ok(layout)
    }
}

/// Serialize the chosen headings only when the result includes a header row.
fn output_header(layout: &OutputLayout, delimiter: u8, config: JoinConfig) -> JResult<Vec<u8>> {
    if !layout.include_header {
        return Ok(Vec::new());
    }
    encode(
        layout.headers.iter().map(|name| name.as_bytes()),
        delimiter,
        config.max_record_bytes,
    )
}

/// Pad absent fields and serialize values without changing their contents.
fn output_record(
    inputs: &[PreparedInput; 2],
    left: Option<&[u8]>,
    right: Option<&[u8]>,
    output: &OutputLayout,
    config: JoinConfig,
) -> JResult<Vec<u8>> {
    let a = left
        .map(|record| {
            fields(
                record,
                inputs[0].info.delimiter,
                false,
                &inputs[0].info.path,
                0,
                config,
            )
        })
        .transpose()?
        .unwrap_or_default();
    let b = right
        .map(|record| {
            fields(
                record,
                inputs[1].info.delimiter,
                false,
                &inputs[1].info.path,
                0,
                config,
            )
        })
        .transpose()?
        .unwrap_or_default();
    if a.len() > inputs[0].info.columns.len() || b.len() > inputs[1].info.columns.len() {
        return Err(error("Prepared join row exceeds the analyzed schema.").into());
    }
    let values = output.columns.iter().map(|column| {
        let row = if column.source == JoinSource::Main {
            &a
        } else {
            &b
        };
        row.get(column.column)
            .map_or(b"".as_slice(), |value| value.as_ref())
    });
    encode(values, inputs[0].info.delimiter, config.max_record_bytes)
}

/// A duplicate group is a byte range in an immutable private sorted run.
/// Reusing that backing file avoids copying or buffering the full group.
#[derive(Clone, Copy)]
struct GroupRange {
    start: u64,
    end: u64,
    rows: u64,
    value_bytes: u64,
    max_value_bytes: usize,
}

struct Group {
    key: Vec<u8>,
    range: GroupRange,
}

impl Group {
    fn into_range(self) -> GroupRange {
        let Self { key, range } = self;
        drop(key);
        range
    }
}

struct OwnedRow {
    record: Vec<u8>,
    ordinal: u64,
}

/// Compute exactly the escaping contribution of one value in its output position.
fn encoded_value_bytes(value: &[u8], delimiter: u8, first: bool) -> JResult<usize> {
    let quote = value
        .iter()
        .any(|b| [delimiter, b'"', b'\r', b'\n'].contains(b))
        || (first && value.starts_with(b"\xef\xbb\xbf"));
    value
        .len()
        .checked_add(if quote {
            2 + value.iter().filter(|b| **b == b'"').count()
        } else {
            0
        })
        .ok_or_else(|| error("Joined value length overflow.").into())
}

/// Separate value bytes from the fixed per-row delimiter and LF contribution.
fn row_value_bytes(
    input: &PreparedInput,
    _side: usize,
    row: &OwnedRow,
    config: JoinConfig,
) -> JResult<usize> {
    let values = fields(
        &row.record,
        input.info.delimiter,
        false,
        &input.info.path,
        row.ordinal + 1,
        config,
    )?;
    if values.len() > input.info.columns.len() {
        return Err(error("Prepared row exceeds the analyzed schema.").into());
    }
    let mut size = 0_usize;
    for &(column, first) in &input.projection {
        let value = values
            .get(column)
            .map_or(b"".as_slice(), |value| value.as_ref());
        size = size
            .checked_add(encoded_value_bytes(value, input.output_delimiter, first)?)
            .ok_or_else(|| error("Joined row length overflow."))?;
    }
    Ok(size)
}

/// Verify the stored comparison key against the decoded record before using it.
fn read_row(
    reader: &mut impl Read,
    input: &PreparedInput,
    key: &[u8],
    record_len: usize,
    ordinal: u64,
    config: JoinConfig,
    state: &Progress,
) -> JResult<OwnedRow> {
    if ordinal >= input.info.data_rows {
        return Err(error("Invalid private source ordinal.").into());
    }
    let record = read_record(reader, record_len, config, state)?;
    let values = fields(
        &record,
        input.info.delimiter,
        false,
        &input.info.path,
        ordinal + 1,
        config,
    )?;
    let matches = key_matches(key, &values, &input.key_columns, ordinal, config.case);
    if !matches {
        return Err(error("Private join key differs from its record.").into());
    }
    drop(values);
    Ok(OwnedRow { record, ordinal })
}

struct Groups<'a> {
    input: &'a PreparedInput,
    side: usize,
    reader: BufReader<File>,
    config: JoinConfig,
    state: &'a Progress,
    rows: u64,
}

impl<'a> Groups<'a> {
    fn new(
        input: &'a PreparedInput,
        side: usize,
        config: JoinConfig,
        state: &'a Progress,
    ) -> JResult<Self> {
        Ok(Self {
            input,
            side,
            reader: BufReader::new(input.sorted.open()?),
            config,
            state,
            rows: 0,
        })
    }

    /// Scan each group once for counts and exact byte-size contributions.
    fn next(&mut self) -> JResult<Option<Group>> {
        self.state.check()?;
        let start = self.reader.stream_position()?;
        let Some(first) = read_head(&mut self.reader, self.config, self.state)? else {
            if self.rows != self.input.info.data_rows {
                return Err(error("Prepared join row count changed.").into());
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
        let mut head = (record_len, ordinal);
        let mut rows = 0;
        let mut value_bytes = 0;
        let mut max_value_bytes = 0;
        let mut last_ordinal = None;
        let end;
        loop {
            self.state.check()?;
            if last_ordinal.is_some_and(|last| head.1 <= last) {
                return Err(error("Private join ordinals are out of order.").into());
            }
            let row = read_row(
                &mut self.reader,
                self.input,
                &key,
                head.0,
                head.1,
                self.config,
                self.state,
            )?;
            let size = row_value_bytes(self.input, self.side, &row, self.config)?;
            value_bytes = add(value_bytes, size as u64)?;
            max_value_bytes = max_value_bytes.max(size);
            rows = add(rows, 1)?;
            last_ordinal = Some(head.1);
            drop(row);
            if rows > 1 && key[0] == 1 && self.input.require_unique {
                return Err(input_error(
                    &self.input.info.path,
                    Some(head.1 + 2),
                    "Repeated key found, but Require unique key is enabled for this input.",
                )
                .into());
            }
            let next_position = self.reader.stream_position()?;
            let Some(next) = read_head(&mut self.reader, self.config, self.state)? else {
                end = next_position;
                break;
            };
            if next.key != key {
                if next.key < key {
                    return Err(error("Private join groups are out of order.").into());
                }
                self.reader.seek(SeekFrom::Start(next_position))?;
                end = next_position;
                break;
            }
            head = (next.record_len, next.ordinal);
        }
        self.rows = add(self.rows, rows)?;
        Ok(Some(Group {
            key,
            range: GroupRange {
                start,
                end,
                rows,
                value_bytes,
                max_value_bytes,
            },
        }))
    }
}

/// Merge only group metadata; the potentially enormous groups remain on disk.
fn walk_groups(
    inputs: &[PreparedInput; 2],
    config: JoinConfig,
    state: &Progress,
    mut visit: impl FnMut(Option<Group>, Option<Group>) -> JResult<()>,
) -> JResult<()> {
    let mut left = Groups::new(&inputs[0], 0, config, state)?;
    let mut right = Groups::new(&inputs[1], 1, config, state)?;
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

struct RangeRows<'a> {
    input: &'a PreparedInput,
    range: GroupRange,
    reader: BufReader<File>,
    consumed: u64,
    config: JoinConfig,
    state: &'a Progress,
}

impl<'a> RangeRows<'a> {
    fn new(
        input: &'a PreparedInput,
        range: GroupRange,
        config: JoinConfig,
        state: &'a Progress,
    ) -> JResult<Self> {
        let mut reader = BufReader::new(input.sorted.open()?);
        reader.seek(SeekFrom::Start(range.start))?;

        Ok(Self {
            input,
            range,
            reader,
            consumed: 0,
            config,
            state,
        })
    }

    fn next(&mut self) -> JResult<Option<OwnedRow>> {
        self.state.check()?;
        let position = self.reader.stream_position()?;
        if position == self.range.end {
            if self.consumed != self.range.rows {
                return Err(error("Private duplicate group changed.").into());
            }
            return Ok(None);
        }
        if position > self.range.end || self.consumed >= self.range.rows {
            return Err(error("Invalid private duplicate-group range.").into());
        }
        let head = read_head(&mut self.reader, self.config, self.state)?
            .ok_or_else(|| error("Truncated duplicate group."))?;
        let row = read_row(
            &mut self.reader,
            self.input,
            &head.key,
            head.record_len,
            head.ordinal,
            self.config,
            self.state,
        )?;
        self.consumed += 1;
        if self.reader.stream_position()? > self.range.end {
            return Err(error("Private duplicate group overran its range.").into());
        }
        Ok(Some(row))
    }

    /// Peek only the next frame length, rewinding before holding any row block.
    fn next_size(&mut self) -> JResult<Option<usize>> {
        let position = self.reader.stream_position()?;
        if position == self.range.end {
            if self.consumed != self.range.rows {
                return Err(error("Private duplicate group changed.").into());
            }
            return Ok(None);
        }
        let head = read_head(&mut self.reader, self.config, self.state)?
            .ok_or_else(|| error("Truncated duplicate group."))?;
        let bytes = head
            .record_len
            .saturating_add(std::mem::size_of::<OwnedRow>());
        drop(head);
        self.reader.seek(SeekFrom::Start(position))?;
        Ok(Some(bytes))
    }

    /// A single oversized record is allowed, within the validated record budget.
    fn block(&mut self) -> JResult<Vec<OwnedRow>> {
        let mut rows = Vec::new();
        let mut bytes = 0_usize;
        while let Some(size) = self.next_size()? {
            if !rows.is_empty() && bytes.saturating_add(size) > self.config.group_memory_bytes {
                break;
            }
            let row = self
                .next()?
                .ok_or_else(|| error("Duplicate group ended unexpectedly."))?;
            rows.try_reserve_exact(1)
                .map_err(|_| error("Cannot allocate the bounded duplicate block."))?;
            bytes = bytes
                .saturating_add(row.record.capacity())
                .saturating_add(std::mem::size_of::<OwnedRow>());
            rows.push(row);
        }
        self.state.group_peak.fetch_max(bytes, Ordering::AcqRel);
        Ok(rows)
    }
}

/// Stream retained pairs using bounded blocks of the side that drives public order.
fn enumerate_pairs(
    inputs: &[PreparedInput; 2],
    left: Option<GroupRange>,
    right: Option<GroupRange>,
    config: JoinConfig,
    state: &Progress,
    limit: Option<usize>,
    mut emit: impl FnMut(Option<&OwnedRow>, Option<&OwnedRow>) -> JResult<()>,
) -> JResult<()> {
    if !config.mode.retains(left.is_some(), right.is_some()) {
        return Ok(());
    }
    let mut config = config;
    if limit.is_some() {
        config.group_memory_bytes = 1;
    }
    let primary = usize::from(left.is_none() || config.mode == JoinMode::Right);
    let ranges = [left, right];
    let mut a = RangeRows::new(&inputs[primary], ranges[primary].unwrap(), config, state)?;
    let mut emitted = 0_usize;
    let mut visit = |first: &OwnedRow, second: Option<&OwnedRow>| -> JResult<bool> {
        #[cfg(test)]
        crate::join_faults::observe(crate::join_faults::Event::ExpandingPairs);
        state.check()?;
        if primary == 0 {
            emit(Some(first), second)?;
        } else {
            emit(second, Some(first))?;
        }
        emitted = emitted.saturating_add(1);
        Ok(limit.is_some_and(|limit| emitted >= limit))
    };
    if let Some(other) = ranges[1 - primary] {
        loop {
            let block = a.block()?;
            if block.is_empty() {
                break;
            }
            let mut b = RangeRows::new(&inputs[1 - primary], other, config, state)?;
            while let Some(row) = b.next()? {
                for first in &block {
                    if visit(first, Some(&row))? {
                        return Ok(());
                    }
                }
            }
        }
    } else {
        while let Some(row) = a.next()? {
            if visit(&row, None)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn pair_order(mode: JoinMode, main: Option<u64>, additional: Option<u64>) -> (u64, u64) {
    let main = main.unwrap_or(u64::MAX);
    let additional = additional.unwrap_or(u64::MAX);
    if mode == JoinMode::Right {
        (additional, main)
    } else {
        (main, additional)
    }
}

fn pair_key(order: (u64, u64)) -> Vec<u8> {
    let mut key = Vec::with_capacity(16);
    key.extend_from_slice(&order.0.to_be_bytes());
    key.extend_from_slice(&order.1.to_be_bytes());
    key
}

/// Validate ordinal bounds and retained-side rules before publication.
fn valid_pair_order(key: &[u8], ordinal: u64, mode: JoinMode, rows: [u64; 2]) -> bool {
    if key.len() != 16 {
        return false;
    }
    let first = u64::from_be_bytes(key[..8].try_into().unwrap());
    let second = u64::from_be_bytes(key[8..].try_into().unwrap());
    if first != ordinal {
        return false;
    }
    let (main, additional) = if mode == JoinMode::Right {
        (second, first)
    } else {
        (first, second)
    };
    let main_present = main != u64::MAX;
    let additional_present = additional != u64::MAX;
    mode.retains(main_present, additional_present)
        && (!main_present || main < rows[0])
        && (!additional_present || additional < rows[1])
}

/// Bound displayed columns and text independently of full result values.
fn preview_row(
    record: &[u8],
    delimiter: u8,
    limit: usize,
    config: JoinConfig,
) -> JResult<Vec<String>> {
    let parsed = fields(
        record,
        delimiter,
        false,
        Path::new("joined preview"),
        0,
        config,
    )?;
    let mut remaining = limit;
    let mut output = vec![];
    for field in parsed.into_iter().take(20) {
        let text = std::str::from_utf8(&field).expect("validated text");
        let mut end = text.len().min(remaining.min(256));
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let value = text[..end].to_string();
        remaining = remaining.saturating_sub(value.len());
        output.push(value);
    }
    Ok(output)
}

/// Compute group expansion before allocating or generating any result rows.
fn matched_group_output(
    a: GroupRange,
    b: GroupRange,
    columns: u64,
    limit: usize,
) -> JResult<(u64, u64)> {
    let count = mul(a.rows, b.rows)?;
    let bytes = add(
        add(mul(a.value_bytes, b.rows)?, mul(b.value_bytes, a.rows)?)?,
        mul(count, columns)?,
    )?;
    let max = a
        .max_value_bytes
        .checked_add(b.max_value_bytes)
        .and_then(|n| n.checked_add(columns as usize))
        .ok_or_else(|| error("Joined record length overflow."))?;
    if max > limit {
        return Err(error("A joined record exceeds the configured size limit.").into());
    }
    Ok((count, bytes))
}

/// Analyze group cardinality and byte bounds without materializing the Cartesian result.
fn analyze_prepared(
    inputs: &[PreparedInput; 2],
    output: &OutputLayout,
    config: JoinConfig,
    state: &Progress,
) -> JResult<JoinAnalysis> {
    state.phase(JoinPhase::Analyzing);
    let header_bytes = output_header(output, inputs[0].info.delimiter, config)?.len() as u64;
    let columns = output.columns.len() as u64;
    let mut matching = [0, 0];
    let mut pairs = 0;
    let mut unmatched = [0, 0];
    let mut duplicate_groups = [0, 0];
    let mut many_to_many_groups = 0;
    let mut many_to_many_pairs = 0;
    let mut duplicate_samples = Vec::new();
    let mut output_rows = 0;
    let mut output_bytes = header_bytes;
    let mut preview = std::collections::BTreeMap::new();
    walk_groups(inputs, config, state, |left, right| {
        for (side, group) in [left.as_ref(), right.as_ref()].into_iter().enumerate() {
            if group.is_some_and(|group| group.key[0] == 1 && group.range.rows > 1) {
                duplicate_groups[side] = add(duplicate_groups[side], 1)?;
            }
        }
        if duplicate_samples.len() < 8
            && (left.as_ref().is_some_and(|g| g.range.rows > 1)
                || right.as_ref().is_some_and(|g| g.range.rows > 1))
        {
            let key = &left.as_ref().or(right.as_ref()).unwrap().key;
            duplicate_samples.push(JoinDuplicateExample {
                key: key_label(key)?,
                main_rows: left.as_ref().map_or(0, |g| g.range.rows),
                additional_rows: right.as_ref().map_or(0, |g| g.range.rows),
            });
        }
        let left = left.map(Group::into_range);
        let right = right.map(Group::into_range);
        match (left, right) {
            (Some(a), Some(b)) => {
                let (count, bytes) = matched_group_output(a, b, columns, config.max_record_bytes)?;
                matching[0] = add(matching[0], a.rows)?;
                matching[1] = add(matching[1], b.rows)?;
                pairs = add(pairs, count)?;
                if a.rows > 1 && b.rows > 1 {
                    many_to_many_groups = add(many_to_many_groups, 1)?;
                    many_to_many_pairs = add(many_to_many_pairs, count)?;
                }
                output_rows = add(output_rows, count)?;
                output_bytes = add(output_bytes, bytes)?;
            }
            (a, b) => {
                let (side, group) = if let Some(a) = a {
                    (0, a)
                } else {
                    (1, b.unwrap())
                };
                unmatched[side] = add(unmatched[side], group.rows)?;
                if config.mode.retains(a.is_some(), b.is_some()) {
                    output_rows = add(output_rows, group.rows)?;
                    output_bytes = add(
                        output_bytes,
                        add(group.value_bytes, mul(group.rows, columns)?)?,
                    )?;
                    if group.max_value_bytes.saturating_add(columns as usize)
                        > config.max_record_bytes
                    {
                        return Err(
                            error("A joined record exceeds the configured size limit.").into()
                        );
                    }
                }
            }
        }
        if config.preview_rows > 0 {
            enumerate_pairs(
                inputs,
                left,
                right,
                config,
                state,
                Some(config.preview_rows),
                |a, b| {
                    let row = output_record(
                        inputs,
                        a.map(|a| a.record.as_slice()),
                        b.map(|b| b.record.as_slice()),
                        output,
                        config,
                    )?;
                    let key = pair_order(config.mode, a.map(|a| a.ordinal), b.map(|b| b.ordinal));
                    if preview.len() < config.preview_rows
                        || preview
                            .last_key_value()
                            .is_some_and(|(&last, _)| key < last)
                    {
                        preview.insert(
                            key,
                            preview_row(
                                &row,
                                inputs[0].info.delimiter,
                                config.preview_bytes / config.preview_rows,
                                config,
                            )?,
                        );
                        if preview.len() > config.preview_rows {
                            preview.pop_last();
                        }
                    }
                    Ok(())
                },
            )?;
        }
        state
            .records
            .store(add(matching[0], unmatched[0])?, Ordering::Release);
        Ok(())
    })?;
    let frames = add(output_bytes - header_bytes, mul(output_rows, 40)?)?;
    let temporary = add(mul(frames, 2)?, output_bytes)?;
    for input in inputs {
        check_source(input)?;
    }
    Ok(JoinAnalysis {
        input_rows: [inputs[0].info.data_rows, inputs[1].info.data_rows],
        matching_rows: matching,
        matching_pairs: pairs,
        unmatched_rows: unmatched,
        empty_key_rows: [inputs[0].info.empty_key_rows, inputs[1].info.empty_key_rows],
        duplicate_groups,
        many_to_many_groups,
        many_to_many_pairs,
        duplicate_samples,
        output_rows,
        output_bytes,
        additional_temporary_bytes: temporary,
        preview: preview.into_values().collect(),
        output_headers: Arc::clone(&output.headers),
        include_header: output.include_header,
        duplicate_headers: output.duplicate_headers,
        new_columns: output.new_columns,
    })
}

/// Keep the original failure visible when private-file cleanup also fails.
fn cleanup_failure(workspace: SpillWorkspace, failure: Failure) -> Failure {
    match workspace.cleanup() {
        Ok(()) => failure,
        Err(cleanup) => {
            let cause = failure.description();
            error(format!(
                "{cause}. Could not remove private join data: {cleanup}"
            ))
            .into()
        }
    }
}

/// Analyze both complete inputs and retain private sorted streams for creation.
pub fn analyze_join(
    inputs: [JoinInput; 2],
    temporary: PathBuf,
    config: JoinConfig,
) -> Result<JoinJob<JoinPlan>, QuarryError> {
    analyze_join_with_output(inputs, temporary, config, JoinOutputOptions::default())
}

/// Analyze a selected output projection without changing either source.
pub fn analyze_join_with_output(
    inputs: [JoinInput; 2],
    temporary: PathBuf,
    config: JoinConfig,
    output_options: JoinOutputOptions,
) -> Result<JoinJob<JoinPlan>, QuarryError> {
    analyze_join_with_working(inputs, [None, None], temporary, config, output_options)
}

/// Analyze saved files and/or frozen current document values through one worker contract.
pub fn analyze_join_with_working(
    inputs: [JoinInput; 2],
    working: [Option<JoinWorkingSource>; 2],
    temporary: PathBuf,
    config: JoinConfig,
    output_options: JoinOutputOptions,
) -> Result<JoinJob<JoinPlan>, QuarryError> {
    config.validate(&temporary)?;
    let pair_count = inputs[0].key_columns.len();
    if !(1..=MAX_KEY_PAIRS).contains(&pair_count) || inputs[1].key_columns.len() != pair_count {
        return Err(error("Select one to sixteen matching key-column pairs."));
    }
    if output_options
        .columns
        .as_ref()
        .is_some_and(|columns| columns.is_empty() || columns.len() > config.max_columns)
    {
        return Err(error(
            "Select at least one output column within the supported column limit.",
        ));
    }
    JoinJob::spawn(move |state| {
        let [left, right] = inputs;
        let [left_working, right_working] = working;
        let left = inspect(left, left_working, config, state)?;
        let mut right = inspect(right, right_working, config, state)?;
        right.output_delimiter = left.info.delimiter;
        if same_input(&left, &right)? {
            return Err(
                error("Choose two different input files. Self joins are not supported.").into(),
            );
        }
        *state
            .discovered
            .lock()
            .map_err(|_| error("Cannot publish discovered source metadata."))? = Some([
            discovered_header(&left, config),
            discovered_header(&right, config),
        ]);
        let output = OutputLayout::resolve(
            [&left.info, &right.info],
            output_options,
            config,
            &temporary,
            state,
        )?;
        let initial = mul(add(left.frame_bytes, right.frame_bytes)?, 2)?;
        check_storage(&temporary, initial)?;
        state.check()?;
        let workspace = SpillWorkspace::create_in(&temporary, "join")?;
        let prepared = (|| {
            let left = prepare(left, &workspace, config, state)?;
            let right = prepare(right, &workspace, config, state)?;
            let mut inputs = [left, right];
            for (position, column) in output.columns.iter().enumerate() {
                inputs[column.source.index()]
                    .projection
                    .push((column.column, position == 0));
            }
            let analysis = analyze_prepared(&inputs, &output, config, state)?;
            Ok((inputs, analysis))
        })();
        match prepared {
            Ok((inputs, analysis)) => Ok(JoinPlan {
                inputs,
                analysis,
                output,
                workspace: Some(workspace),
                config,
            }),
            Err(failure) => Err(cleanup_failure(workspace, failure)),
        }
    })
}

/// Account for simultaneous staging and ordering files on their actual volumes.
fn check_creation_space(
    temporary: &Path,
    destination: &Path,
    spill_bytes: u64,
    output_bytes: u64,
) -> JResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let same_volume =
            fs::metadata(temporary)?.dev() == fs::metadata(output_parent(destination))?.dev();
        if same_volume {
            check_storage(temporary, add(spill_bytes, output_bytes)?)?;
        } else {
            check_storage(temporary, spill_bytes)?;
            check_storage(output_parent(destination), output_bytes)?;
        }
    }
    #[cfg(not(unix))]
    {
        // Unknown volume identity requires checking both locations. Reuse the
        // shared conservative fallback rather than inspecting only scratch space.
        crate::storage::check_operation_storage(
            temporary,
            destination,
            add(spill_bytes, output_bytes)?,
            output_bytes,
        )?;
    }
    Ok(())
}

impl JoinPlan {
    /// Read exact analysis without permitting a caller to alter the retained plan.
    pub fn analysis(&self) -> &JoinAnalysis {
        &self.analysis
    }

    pub fn inputs(&self) -> [&JoinInputInfo; 2] {
        [&self.inputs[0].info, &self.inputs[1].info]
    }

    /// Release an idle preview explicitly, reporting filesystem cleanup failures.
    pub fn release(mut self) -> Result<(), QuarryError> {
        self.workspace.take().expect("join workspace").cleanup()
    }

    /// Create a result when no many-to-many acknowledgement is required.
    pub fn start_write(self, destination: PathBuf) -> Result<JoinJob<JoinSummary>, QuarryError> {
        self.start_write_acknowledged(destination, false)
    }

    /// Acknowledgement is tied to this immutable, fully analyzed plan.
    pub fn start_write_acknowledged(
        self,
        destination: PathBuf,
        acknowledge_expansion: bool,
    ) -> Result<JoinJob<JoinSummary>, QuarryError> {
        self.start_write_reviewed(destination, acknowledge_expansion, false)
    }

    pub fn start_write_reviewed(
        mut self,
        destination: PathBuf,
        acknowledge_expansion: bool,
        acknowledge_layout: bool,
    ) -> Result<JoinJob<JoinSummary>, QuarryError> {
        if self.analysis.new_columns != [0, 0] && !acknowledge_layout {
            self.release()?;
            return Err(error(
                "Review the newly discovered output columns before creating the result.",
            ));
        }
        if self.analysis.many_to_many_groups > 0 && !acknowledge_expansion {
            self.release()?;
            return Err(error(
                "Acknowledge the exact many-to-many row expansion before creating this result.",
            ));
        }
        let workspace = self.workspace.take().expect("join workspace");
        JoinJob::spawn(move |state| {
            let result = self.write_inner(&workspace, &destination, state);
            match result {
                Ok(mut summary) => {
                    if let Err(error) = workspace.cleanup() {
                        summary.cleanup_warning = Some(match summary.cleanup_warning.take() {
                            Some(previous) => format!("{previous}; {error}"),
                            None => error.to_string(),
                        });
                    }
                    Ok(summary)
                }
                Err(failure) => Err(cleanup_failure(workspace, failure)),
            }
        })
    }

    /// Shared publication guards report CombineFile for multiple sources. Keep
    /// a join failure tied to its logical input and explain the recovery action.
    fn publication_error(&self, error: QuarryError, phase: JoinPhase) -> QuarryError {
        match error {
            QuarryError::CombineFile {
                path,
                record,
                reason,
            } => {
                let source = self.inputs.iter().enumerate().find(|(_, input)| {
                    input.info.path == path
                        || input.physical_path == path
                        || input.working.as_ref().is_some_and(|working| {
                            working.guards().iter().any(|(guard, _)| guard == &path)
                        })
                });
                match source {
                    Some((side, input)) => input_error(
                        &input.info.path,
                        record,
                        format!(
                            "{} input changed after analysis. Review this input and analyze again.",
                            if side == 0 { "Main" } else { "Additional" }
                        ),
                    ),
                    None => input_error(&path, record, reason),
                }
            }
            QuarryError::Storage { directory, error } => QuarryError::Storage {
                directory,
                error: io::Error::new(
                    error.kind(),
                    format!(
                        "{}: {error}. Check free space and folder permissions, then analyze again",
                        phase.label()
                    ),
                ),
            },
            error => error,
        }
    }

    /// Validate count, shape and byte size before guarded atomic publication.
    fn write_inner(
        &self,
        workspace: &SpillWorkspace,
        destination: &Path,
        state: &Progress,
    ) -> JResult<JoinSummary> {
        state.phase(JoinPhase::Creating);
        state.check()?;
        let prepared_destination =
            crate::destination::PreparedDestination::new(destination.to_path_buf())?;
        workspace.ensure_output_outside(&prepared_destination)?;
        for input in &self.inputs {
            if let Some(source) = &input.working {
                source.ensure_output_outside(&prepared_destination)?;
            }
        }
        let retained = add(self.inputs[0].frame_bytes, self.inputs[1].frame_bytes)?;
        state.add_disk(retained)?;
        for input in &self.inputs {
            check_source(input)?;
        }
        let header = output_header(&self.output, self.inputs[0].info.delimiter, self.config)?;
        let frame_bytes = add(
            self.analysis.output_bytes - header.len() as u64,
            mul(self.analysis.output_rows, 40)?,
        )?;
        check_creation_space(
            &workspace.path,
            &prepared_destination.physical_path(),
            mul(frame_bytes, 2)?,
            self.analysis.output_bytes,
        )?;
        let sources = self
            .inputs
            .iter()
            .flat_map(|input| {
                let mut guards = vec![(input.physical_path.clone(), input.stamp.clone())];
                if let Some(working) = &input.working {
                    guards.extend(working.guards());
                }
                guards
            })
            .collect();
        let mut output = ExportTarget::new_combined_at(sources, prepared_destination)
            .map_err(|error| self.publication_error(error, JoinPhase::Creating))?;
        output.configure(Encoding::Utf8, Encoding::Utf8, self.config.max_record_bytes);
        let prepared = (|| {
            output
                .write_all(&header)
                .map_err(|error| self.publication_error(error, JoinPhase::Creating))?;
            state.add_disk(header.len() as u64)?;
            drop(header);
            let mut runs = Runs::new(workspace, self.config, state, 16);
            let mut count = 0;
            let mut bytes = 0;
            walk_groups(&self.inputs, self.config, state, |left, right| {
                enumerate_pairs(
                    &self.inputs,
                    left.map(Group::into_range),
                    right.map(Group::into_range),
                    self.config,
                    state,
                    None,
                    |left, right| {
                        let row = output_record(
                            &self.inputs,
                            left.map(|r| r.record.as_slice()),
                            right.map(|r| r.record.as_slice()),
                            &self.output,
                            self.config,
                        )?;
                        count = add(count, 1)?;
                        bytes = add(bytes, row.len() as u64)?;
                        let order = pair_order(
                            self.config.mode,
                            left.map(|r| r.ordinal),
                            right.map(|r| r.ordinal),
                        );
                        runs.push(Entry {
                            key: pair_key(order),
                            record: row,
                            ordinal: order.0,
                        })?;
                        state.records.store(count, Ordering::Release);
                        Ok(())
                    },
                )?;
                Ok(())
            })?;
            #[cfg(test)]
            crate::join_faults::observe(crate::join_faults::Event::RestoringOrder);
            let ordered = runs.finish()?;
            if count != self.analysis.output_rows
                || add(bytes, output.bytes_written())? != self.analysis.output_bytes
                || ordered.bytes != frame_bytes
            {
                return Err(
                    error("Join output differs from the analyzed plan. Analyze again.").into(),
                );
            }
            let mut reader = BufReader::new(ordered.path.open()?);
            let mut ordinal = 0;
            let mut previous_order: Option<Vec<u8>> = None;
            while let Some(head) = read_head(&mut reader, self.config, state)? {
                if !valid_pair_order(
                    &head.key,
                    head.ordinal,
                    self.config.mode,
                    [self.inputs[0].info.data_rows, self.inputs[1].info.data_rows],
                ) || previous_order
                    .as_ref()
                    .is_some_and(|previous| previous >= &head.key)
                {
                    return Err(error("Joined row pairs are not in source order.").into());
                }
                previous_order = Some(head.key);
                let record = read_record(&mut reader, head.record_len, self.config, state)?;
                let parsed = fields(
                    &record,
                    self.inputs[0].info.delimiter,
                    false,
                    destination,
                    ordinal,
                    self.config,
                )?;
                if parsed.len() != self.output.columns.len() {
                    return Err(error("Joined row has the wrong number of columns.").into());
                }
                drop(parsed);
                #[cfg(test)]
                crate::join_faults::observe(crate::join_faults::Event::WritingRow);
                output
                    .write_all(&record)
                    .map_err(|error| self.publication_error(error, JoinPhase::Creating))?;
                state.add_disk(record.len() as u64)?;
                ordinal = add(ordinal, 1)?;
            }
            if ordinal != self.analysis.output_rows
                || output.bytes_written() != self.analysis.output_bytes
            {
                return Err(
                    error("Joined output count or size changed before publication.").into(),
                );
            }
            for input in &self.inputs {
                check_source(input)?;
            }
            Ok(())
        })();
        if let Err(failure) = prepared {
            return Err(match output.discard() {
                Ok(()) => failure,
                Err(cleanup) => error(format!(
                    "{}. Could not remove output staging: {cleanup}",
                    failure.description()
                ))
                .into(),
            });
        }
        state.phase(JoinPhase::Publishing);
        let (outcome, cleanup_warning) = output
            .publish_with_cleanup(
                self.analysis.output_rows,
                self.analysis.output_bytes,
                &state.cancel,
            )
            .map_err(|failure| {
                let cause = self.publication_error(failure.cause, JoinPhase::Publishing);
                match failure.cleanup {
                    Some(cleanup) => error(format!(
                        "{}. Could not remove output staging: {cleanup}",
                        cause.to_string().trim_end_matches('.')
                    )),
                    None => cause,
                }
            })?;
        match outcome {
            FilterExportOutcome::Cancelled => Err(Failure::Cancelled),
            FilterExportOutcome::Complete(summary) => Ok(JoinSummary {
                destination: summary.destination,
                data_rows: self.analysis.output_rows,
                bytes_written: self.analysis.output_bytes,
                include_header: self.output.include_header,
                peak_temporary_bytes: state.peak.load(Ordering::Acquire),
                peak_group_buffer_bytes: state.group_peak.load(Ordering::Acquire),
                cleanup_warning,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    #[test]
    fn retained_workspace_replacement_never_deletes_a_substituted_tree() {
        use std::os::unix::fs::PermissionsExt;
        for action in ["release", "drop", "create"] {
            let dir = tempfile::tempdir().unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
            let source = inputs(
                dir.path(),
                b"id,value\nx,Main\n",
                b"id,value\nx,Additional\n",
            );
            let plan = analyze_join(source, dir.path().to_path_buf(), config())
                .unwrap()
                .wait()
                .unwrap()
                .unwrap();
            let workspace = plan.workspace.as_ref().unwrap().path.clone();
            let moved = dir.path().join("moved-private");
            let victim = dir.path().join("victim");
            fs::create_dir_all(victim.join("nested")).unwrap();
            fs::write(victim.join("nested/keep.csv"), b"unrelated,data\nkeep,me\n").unwrap();
            fs::rename(&workspace, &moved).unwrap();
            fs::rename(&victim, &workspace).unwrap();
            match action {
                "release" => {
                    plan.release().unwrap();
                }
                "drop" => drop(plan),
                _ => {
                    let output = dir.path().join("joined.csv");
                    plan.start_write(output.clone())
                        .unwrap()
                        .wait()
                        .unwrap()
                        .unwrap();
                    assert_eq!(fs::read(output).unwrap(),
                        b"id (Main),value (Main),id (Additional),value (Additional)\nx,Main,x,Additional\n");
                }
            }
            assert_eq!(
                fs::read(workspace.join("nested/keep.csv")).unwrap(),
                b"unrelated,data\nkeep,me\n",
                "cleanup must leave the substituted tree untouched"
            );
            assert!(
                !moved.exists(),
                "the captured workspace itself should be cleaned"
            );
            assert_eq!(
                fs::read(dir.path().join("main.csv")).unwrap(),
                b"id,value\nx,Main\n"
            );
        }
    }

    #[cfg(unix)]
    fn replace_with_fifo(path: &Path) {
        fs::remove_file(path).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    }

    #[cfg(unix)]
    fn assert_fifo_reopen_returns_promptly<T: Send + 'static>(job: JoinJob<T>, fifo: &Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !job.progress().done && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let prompt = job.progress().done;
        // A failing pre-fix test must unblock and reap its worker rather than
        // leave the suite hanging on a FIFO with no writer.
        let rescue = if !prompt {
            job.cancel();
            Some(
                rustix::fs::open(
                    fifo,
                    rustix::fs::OFlags::RDWR
                        | rustix::fs::OFlags::NONBLOCK
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .unwrap(),
            )
        } else {
            None
        };
        let outcome = job.wait();
        drop(rescue);
        assert!(
            prompt,
            "a source replaced by a FIFO blocked the join worker until a writer rescued it"
        );
        assert!(
            outcome.is_err(),
            "a non-regular replacement must be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fifo_replacement_after_probe_does_not_block_analysis_or_close() {
        let dir = tempfile::tempdir().unwrap();
        let mut sources = inputs(
            dir.path(),
            b"id,value\nx,Main\n",
            b"id,value\nx,Additional\n",
        );
        sources[0].expected_version = Some(
            read_join_header(sources[0].path.clone())
                .unwrap()
                .wait()
                .unwrap()
                .unwrap()
                .source_version,
        );
        let source = sources[0].path.clone();
        replace_with_fifo(&source);
        let job = analyze_join(sources, dir.path().to_path_buf(), config()).unwrap();
        assert_fifo_reopen_returns_promptly(job, &source);
        assert_eq!(
            fs::read(dir.path().join("additional.csv")).unwrap(),
            b"id,value\nx,Additional\n"
        );
        assert!(workspaces(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn fifo_replacement_after_analysis_does_not_block_creation_or_close() {
        let dir = tempfile::tempdir().unwrap();
        let sources = inputs(
            dir.path(),
            b"id,value\nx,Main\n",
            b"id,value\nx,Additional\n",
        );
        let source = sources[0].path.clone();
        let plan = analyze_join(sources, dir.path().to_path_buf(), config())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        replace_with_fifo(&source);
        let output = dir.path().join("joined.csv");
        let job = plan.start_write(output.clone()).unwrap();
        assert_fifo_reopen_returns_promptly(job, &source);
        assert!(!output.exists());
        assert_eq!(
            fs::read(dir.path().join("additional.csv")).unwrap(),
            b"id,value\nx,Additional\n"
        );
        assert!(workspaces(dir.path()).is_empty());
    }

    /// Exercise multiple spill passes with small but valid resource budgets.
    fn config() -> JoinConfig {
        JoinConfig {
            mode: JoinMode::Left,
            case: JoinCase::Sensitive,
            memory_bytes: 512 * 1024,
            run_memory_bytes: 128,
            group_memory_bytes: 64,
            max_record_bytes: 1024,
            max_columns: 16,
            max_merge_fan_in: 2,
            preview_rows: 3,
            preview_bytes: 1024,
        }
    }
    fn inputs(dir: &Path, a: &[u8], b: &[u8]) -> [JoinInput; 2] {
        let paths = [dir.join("main.csv"), dir.join("additional.csv")];
        fs::write(&paths[0], a).unwrap();
        fs::write(&paths[1], b).unwrap();
        paths.map(|path| JoinInput {
            path,
            delimiter: Some(b','),
            has_header: true,
            encoding: Some(Encoding::Utf8),
            key_columns: vec![0],
            expected_version: None,
            require_unique: true,
        })
    }
    /// Exercise analysis through the public worker boundary with synthetic files.
    fn plan(dir: &Path, a: &[u8], b: &[u8]) -> JoinPlan {
        analyze_join(inputs(dir, a, b), dir.to_path_buf(), config())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap()
    }
    /// Require failed analysis to return an error through the public boundary.
    fn failure(dir: &Path, a: &[u8], b: &[u8]) -> String {
        match analyze_join(inputs(dir, a, b), dir.to_path_buf(), config())
            .unwrap()
            .wait()
        {
            Err(error) => error.to_string(),
            _ => panic!("expected failure"),
        }
    }
    /// Identify owned scratch artifacts without including fixture files.
    fn workspaces(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".quarry-")
            })
            .collect()
    }

    #[test]
    /// Compare exact bytes with a handwritten oracle for quoting, padding and row order.
    fn left_join_preserves_values_missing_fields_and_original_order() {
        let dir = tempfile::tempdir().unwrap();
        let a=b"key,label\nb,\"B, \"\"quoted\"\"\"\na,\"line one\nline two\"\n,empty\n01,leading zero\n1,number text\n ,space\nz\n";
        let b = b"key,label\na,Alpha\nb,Beta\n1,One\n ,Space\nx,Extra\n";
        let plan = plan(dir.path(), a, b);
        assert_eq!(plan.analysis.input_rows, [7, 5]);
        assert_eq!(plan.analysis.matching_rows, [4, 4]);
        assert_eq!(plan.analysis.unmatched_rows, [3, 1]);
        assert_eq!(plan.analysis.empty_key_rows, [1, 0]);
        assert_eq!(plan.analysis.output_rows, 7);
        assert_eq!(
            plan.analysis.preview[0],
            ["b", "B, \"quoted\"", "b", "Beta"]
        );
        let output = dir.path().join("joined.csv");
        let expected=b"key (Main),label (Main),key (Additional),label (Additional)\nb,\"B, \"\"quoted\"\"\",b,Beta\na,\"line one\nline two\",a,Alpha\n,empty,,\n01,leading zero,,\n1,number text,1,One\n ,space, ,Space\nz,,,\n";
        assert_eq!(plan.analysis.output_bytes, expected.len() as u64);
        let result = plan
            .start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(result.data_rows, 7);
        assert_eq!(result.bytes_written, expected.len() as u64);
        assert_eq!(fs::read(output).unwrap(), expected);
        assert_eq!(fs::read(dir.path().join("main.csv")).unwrap(), a);
        assert_eq!(fs::read(dir.path().join("additional.csv")).unwrap(), b);
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    /// Use a separate nested-loop oracle while forcing leveled external merges.
    fn independent_nested_loop_oracle_matches_many_forced_spill_passes() {
        let dir = tempfile::tempdir().unwrap();
        let left: Vec<_> = (0..257)
            .map(|i| format!("{:04},Main-{i}\n", (i * 137) % 521))
            .collect();
        let right: Vec<_> = (0..239)
            .map(|i| format!("{:04},Extra-{i}\n", (i * 47) % 521))
            .collect();
        let a = format!("key,value\n{}", left.concat());
        let b = format!("key,value\n{}", right.concat());
        let mut expected =
            "key (Main),value (Main),key (Additional),value (Additional)\n".to_string();
        let mut matches = 0;
        for l in &left {
            let (key, label) = l.trim_end().split_once(',').unwrap();
            let found = right.iter().find(|r| r.split_once(',').unwrap().0 == key);
            if let Some(r) = found {
                expected.push_str(&format!("{key},{label},{r}"));
                matches += 1;
            } else {
                expected.push_str(&format!("{key},{label},,\n"));
            }
        }
        let plan = plan(dir.path(), a.as_bytes(), b.as_bytes());
        assert_eq!(plan.analysis.matching_pairs, matches);
        assert_eq!(plan.analysis.output_bytes, expected.len() as u64);
        let allowance = plan.analysis.additional_temporary_bytes
            + plan.inputs.iter().map(|p| p.frame_bytes).sum::<u64>();
        let output = dir.path().join("joined.csv");
        let result = plan
            .start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(fs::read_to_string(output).unwrap(), expected);
        assert!(result.peak_temporary_bytes <= allowance);
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    /// Preserve the Main-side contract even when no records can match.
    fn header_only_and_no_matches_are_valid_left_joins() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(dir.path(), b"key,x\n", b"key,y\nz,extra\n");
        assert_eq!(p.analysis.output_rows, 0);
        let output = dir.path().join("empty.csv");
        p.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(output).unwrap(),
            b"key (Main),x,key (Additional),y\n"
        );
        let p = plan(dir.path(), b"key,x\nz,last\na,first\n", b"key,y\n");
        let output = dir.path().join("unmatched.csv");
        p.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(output).unwrap(),
            b"key (Main),x,key (Additional),y\nz,last,,\na,first,,\n"
        );
    }

    #[test]
    /// Separate unmatchable empty values from repeated valid comparison keys.
    fn duplicate_keys_fail_but_empty_keys_never_match_or_fail_uniqueness() {
        let dir = tempfile::tempdir().unwrap();
        assert!(failure(dir.path(), b"id\nx\nx\n", b"id\nx\n").contains("Repeated key"));
        assert!(failure(dir.path(), b"id\nx\n", b"id\nx\nx\n").contains("Repeated key"));
        assert!(workspaces(dir.path()).is_empty());
        let p = plan(dir.path(), b"id\n\n\n", b"id\n\n\n");
        assert_eq!(p.analysis.matching_pairs, 0);
        assert_eq!(p.analysis.empty_key_rows, [2, 2]);
        let output = dir.path().join("empty-keys.csv");
        p.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(output).unwrap(),
            b"id (Main),id (Additional)\n,\n,\n"
        );
    }

    #[test]
    /// Reject malformed data before creating any output, including unused values.
    fn invalid_unselected_text_and_malformed_records_block_analysis() {
        let dir = tempfile::tempdir().unwrap();
        assert!(failure(dir.path(), b"id,label\nx,\xff\n", b"id\nx\n").contains("Invalid UTF-8"));
        assert!(failure(dir.path(), b"id\n\"unfinished", b"id\nx\n").contains("quote"));
        assert!(failure(dir.path(), b"", b"id\nx\n").contains("empty"));
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    /// Verify serialization preserves literal BOM-looking values and distinct headings.
    fn leading_bom_values_and_duplicate_header_names_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(
            dir.path(),
            b"\xef\xbb\xbfid,,id\nx,a,b\n",
            b"id,id (Main)\nx,c\n",
        );
        assert!(p.inputs()[0].columns[1].is_empty());
        let output = dir.path().join("headers.csv");
        p.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        let session = crate::Session::open(
            &output,
            crate::OpenOptions {
                header_mode: crate::HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        let header = &session.first_rows[0].fields;
        assert_eq!(
            header,
            &[
                b"id (Main)".to_vec(),
                b"Main column 2".to_vec(),
                b"id (Main) 2".to_vec(),
                b"id (Additional)".to_vec(),
                b"id (Main) 3".to_vec()
            ]
        );
        let p = plan(
            dir.path(),
            "id,value\n\"\\u{feff}key\",value\n"
                .replace("\\u{feff}", "\u{feff}")
                .as_bytes(),
            "id,value\n\"\\u{feff}key\",other\n"
                .replace("\\u{feff}", "\u{feff}")
                .as_bytes(),
        );
        let output = dir.path().join("bom.csv");
        p.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        let session = crate::Session::open(
            output,
            crate::OpenOptions {
                header_mode: crate::HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(session.first_rows[1].fields[0], "\u{feff}key".as_bytes());
    }

    #[test]
    /// Exercise the publication boundary against source, destination and private-data conflicts.
    fn changed_sources_existing_destinations_and_bad_frames_never_publish() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(dir.path(), b"id\nx\n", b"id\nx\n");
        fs::write(dir.path().join("additional.csv"), b"id\ny\n").unwrap();
        let output = dir.path().join("changed.csv");
        assert!(p.start_write(output.clone()).unwrap().wait().is_err());
        assert!(!output.exists());
        assert!(workspaces(dir.path()).is_empty());
        let p = plan(dir.path(), b"id\nx\n", b"id\nx\n");
        fs::write(&output, b"keep").unwrap();
        assert!(p.start_write(output.clone()).unwrap().wait().is_err());
        assert_eq!(fs::read(&output).unwrap(), b"keep");
        let p = plan(dir.path(), b"id\nx\n", b"id\nx\n");
        fs::write(&p.inputs[0].sorted, [0_u8; 24]).unwrap();
        let output = dir.path().join("corrupt.csv");
        assert!(p.start_write(output.clone()).unwrap().wait().is_err());
        assert!(!output.exists());
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    /// Keep matching independent of presentation and avoid implicit text cleanup.
    fn exact_case_and_whitespace_are_significant_and_key_can_be_later_column() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = inputs(
            dir.path(),
            b"value,id\nupper,A\nlower,a\nspace, a\nmissing\n",
            b"name,key\nmatch,a\n",
        );
        args.iter_mut()
            .for_each(|input| input.key_columns = vec![1]);
        let p = analyze_join(args, dir.path().to_path_buf(), config())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(p.analysis.matching_pairs, 1);
        assert_eq!(p.analysis.empty_key_rows, [1, 0]);
        let output = dir.path().join("case.csv");
        p.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(output).unwrap(),
            b"value,id,name,key\nupper,A,,\nlower,a,match,a\nspace, a,,\nmissing,,,\n"
        );
    }

    #[test]
    /// Fail unsupported requests before creating scratch storage.
    fn explicit_formats_self_joins_and_inadequate_budgets_are_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = inputs(dir.path(), b"id\nx\n", b"id\nx\n");
        args[1].has_header = false;
        let p = analyze_join(args, dir.path().to_path_buf(), config())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert!(!p.inputs()[1].has_header);
        assert_eq!(p.inputs()[1].data_rows, 2);
        p.release().unwrap();
        let mut args = inputs(dir.path(), b"id\nx\n", b"id\nx\n");
        args[1].encoding = Some(Encoding::Latin1);
        let p = analyze_join(args, dir.path().to_path_buf(), config())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(p.inputs()[1].encoding, Encoding::Latin1);
        p.release().unwrap();
        let mut args = inputs(dir.path(), b"id\nx\n", b"id\nx\n");
        args[1].path = args[0].path.clone();
        assert!(
            analyze_join(args, dir.path().to_path_buf(), config())
                .unwrap()
                .wait()
                .is_err()
        );
        let args = inputs(dir.path(), b"id\nx\n", b"id\nx\n");
        let tiny = JoinConfig {
            memory_bytes: 1,
            ..config()
        };
        assert!(analyze_join(args, dir.path().to_path_buf(), tiny).is_err());
        assert!(workspaces(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    /// Verify alias rejection and permissions on a retained real analysis plan.
    fn hardlink_inputs_are_rejected_and_retained_spills_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut args = inputs(dir.path(), b"id\nx\n", b"id\nx\n");
        let alias = dir.path().join("alias.csv");
        fs::hard_link(&args[0].path, &alias).unwrap();
        args[1].path = alias;
        assert!(
            analyze_join(args, dir.path().to_path_buf(), config())
                .unwrap()
                .wait()
                .is_err()
        );
        let p = plan(dir.path(), b"id\nx\n", b"id\nx\n");
        for workspace in workspaces(dir.path()) {
            assert_eq!(
                fs::metadata(&workspace).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for file in fs::read_dir(workspace).unwrap() {
                assert_eq!(
                    file.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        p.release().unwrap();
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    /// Cancel after actual row generation begins and verify owned-file cleanup.
    fn cancelling_a_running_writer_releases_spills_and_keeps_sources() {
        let dir = tempfile::tempdir().unwrap();
        let a = format!(
            "id,value\n{}",
            (0..20000)
                .map(|i| format!("{i:08},value-{i}\n"))
                .collect::<String>()
        );
        let mut cfg = config();
        // Four streams also reserve the bounded ownership registry and paths.
        // Keep the tiny run buffer, so cancellation still crosses spill passes.
        cfg.memory_bytes = 1024 * 1024;
        cfg.run_memory_bytes = 32 * 1024;
        cfg.max_merge_fan_in = 4;
        let args = inputs(dir.path(), a.as_bytes(), b"id,value\nnone,other\n");
        let p = analyze_join(args, dir.path().to_path_buf(), cfg)
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        let output = dir.path().join("cancelled.csv");
        let job = p.start_write(output.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let progress = job.progress();
            assert!(Instant::now() < deadline);
            assert!(
                !progress.done,
                "writer finished before cancellation checkpoint"
            );
            if progress.records > 0 {
                job.cancel();
                break;
            }
            thread::yield_now();
        }
        assert!(job.wait().unwrap().is_none());
        assert!(!output.exists());
        assert!(workspaces(dir.path()).is_empty());
        assert_eq!(fs::read(dir.path().join("main.csv")).unwrap(), a.as_bytes());
    }
    #[test]
    /// Exercise repeated-name disambiguation without restarting suffix scans.
    fn many_identical_headers_have_unique_bounded_names() {
        let dir = tempfile::tempdir().unwrap();
        let header = format!("{}\n", vec!["same"; 2048].join(","));
        let args = inputs(dir.path(), header.as_bytes(), b"extra\n");
        let cfg = JoinConfig {
            memory_bytes: 4 * 1024 * 1024,
            max_record_bytes: 64 * 1024,
            max_columns: 4096,
            ..config()
        };
        let plan = analyze_join(args, dir.path().to_path_buf(), cfg)
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        let output = dir.path().join("wide.csv");
        plan.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        let source = fs::read(output).unwrap();
        let header = quarry_delimited::parse_record(&source, b',').unwrap();
        assert_eq!(header.len(), 2049);
        assert_eq!(header[2047].as_ref(), b"same (Main) 2048");
        assert_eq!(
            header
                .iter()
                .map(|field| field.as_ref())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            2049
        );
    }
    /// A published output must never be removed with the operation's scratch directory.
    #[test]
    fn rejects_public_output_inside_the_owned_spill_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(dir.path(), b"id\nx\n", b"id\nx\n");
        let destination = p.workspace.as_ref().unwrap().path.join("joined.csv");
        let rejected = match p.start_write(destination.clone()) {
            Ok(job) => job.wait().is_err(),
            Err(_) => true,
        };
        assert!(
            rejected,
            "an output in the owned workspace must be rejected, not reported as saved"
        );
        assert!(!destination.exists());
        assert_eq!(fs::read(dir.path().join("main.csv")).unwrap(), b"id\nx\n");
        assert!(workspaces(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_output_through_an_alias_to_the_owned_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan(dir.path(), b"id\nx\n", b"id\nx\n");
        let alias = dir.path().join("chosen-folder");
        std::os::unix::fs::symlink(&p.workspace.as_ref().unwrap().path, &alias).unwrap();
        let destination = alias.join("joined.csv");
        let rejected = match p.start_write(destination.clone()) {
            Ok(job) => job.wait().is_err(),
            Err(_) => true,
        };
        assert!(
            rejected,
            "an alias must not evade the workspace output guard"
        );
        assert!(!destination.exists());
        assert!(workspaces(dir.path()).is_empty());
    }
    #[test]
    fn header_reader_cancellation_stops_future_reads_and_seeks() {
        let state = Progress::new();
        let mut reader = HeaderReader {
            inner: std::io::Cursor::new(vec![b'x'; READ_CHUNK * 2]),
            state: &state,
        };
        let mut buffer = vec![0; READ_CHUNK * 2];
        assert_eq!(reader.read(&mut buffer).unwrap(), READ_CHUNK);
        let position = reader.inner.position();
        state.cancel.store(true, Ordering::Release);
        assert!(reader.read(&mut buffer).is_err());
        assert!(reader.seek(SeekFrom::Start(0)).is_err());
        assert_eq!(reader.inner.position(), position);
        assert!(matches!(
            header_info(Path::new("not-opened.csv"), JoinFormat::default(), &state),
            Err(Failure::Cancelled)
        ));
    }

    #[test]
    fn header_jobs_preserve_detection_and_bound_labels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("headers.tsv");
        fs::write(&path, format!("id\t{}\n1\tvalue\n", "é".repeat(200))).unwrap();
        let header = read_join_header(path.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(header.delimiter, b'\t');
        assert_eq!(header.columns[0], "id");
        assert_eq!(header.columns[1].len(), 256);
        assert!(header.columns.iter().map(String::len).sum::<usize>() <= 512 * 1024);
        fs::write(&path, b"\xff,value\n").unwrap();
        assert!(read_join_header(path.clone()).unwrap().wait().is_err());
        fs::write(&path, b"").unwrap();
        assert!(read_join_header(path).unwrap().wait().is_err());
        assert!(
            read_join_header(dir.path().to_path_buf())
                .unwrap()
                .wait()
                .is_err()
        );
    }
    #[cfg(unix)]
    #[test]
    fn review_regression_parent_retarget_after_validation_preserves_published_result() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let alias = dir.path().join("alias");
        symlink(&outside, &alias).unwrap();
        let source = dir.path().join("source.csv");
        fs::write(&source, b"id\none\n").unwrap();
        let stamp = SourceStamp::from_file(&File::open(&source).unwrap()).unwrap();
        let workspace = SpillWorkspace::create_in(dir.path(), "join").unwrap();
        let prepared =
            crate::destination::PreparedDestination::new(alias.join("joined.csv")).unwrap();
        workspace.ensure_output_outside(&prepared).unwrap();
        // Deterministically schedule the symlink change after validation.
        fs::remove_file(&alias).unwrap();
        symlink(&workspace.path, &alias).unwrap();
        let mut target =
            ExportTarget::new_combined_at(vec![(source.clone(), stamp)], prepared).unwrap();
        target.write_all(b"id\none\n").unwrap();
        let outcome = target.publish(1, 7, &AtomicBool::new(false)).unwrap();
        assert!(matches!(outcome, FilterExportOutcome::Complete(_)));
        workspace.cleanup().unwrap();
        assert_eq!(
            fs::read(outside.join("joined.csv")).unwrap(),
            b"id\none\n",
            "successful output must survive private-workspace cleanup"
        );
        assert_eq!(fs::read(source).unwrap(), b"id\none\n");
    }

    #[cfg(unix)]
    #[test]
    fn review_regression_parent_retarget_after_staging_uses_original_directory() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let other = dir.path().join("other");
        fs::create_dir(&other).unwrap();
        let alias = dir.path().join("alias");
        symlink(&outside, &alias).unwrap();
        let source = dir.path().join("source.csv");
        fs::write(&source, b"id\none\n").unwrap();
        let stamp = SourceStamp::from_file(&File::open(&source).unwrap()).unwrap();
        let mut target =
            ExportTarget::new_combined(vec![(source.clone(), stamp)], alias.join("joined.csv"))
                .unwrap();
        target.write_all(b"id\none\n").unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        let result = target.publish(1, 7, &AtomicBool::new(false));
        assert!(
            matches!(result, Ok(FilterExportOutcome::Complete(_))),
            "retargeting a parent alias must not redirect or strand staged data: {result:?}"
        );
        assert_eq!(fs::read(outside.join("joined.csv")).unwrap(), b"id\none\n");
        assert!(
            fs::read_dir(&other).unwrap().next().is_none(),
            "unrelated folder must be untouched"
        );
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            1,
            "only the result may remain"
        );
        assert_eq!(fs::read(source).unwrap(), b"id\none\n");
    }
    #[cfg(unix)]
    #[test]
    fn pinned_workspace_guard_uses_identity_after_alias_moves_outside() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let workspace = SpillWorkspace::create_in(dir.path(), "join").unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let alias = dir.path().join("alias");
        symlink(&workspace.path, &alias).unwrap();
        let prepared =
            crate::destination::PreparedDestination::new(alias.join("joined.csv")).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&outside, &alias).unwrap();
        assert!(
            workspace.ensure_output_outside(&prepared).is_err(),
            "moving the alias cannot change the selected directory's workspace membership"
        );
        workspace.cleanup().unwrap();
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_publication_cancellation_and_destination_races_preserve_other_files() {
        use std::os::unix::fs::symlink;
        for cancel in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let outside = dir.path().join("outside");
            fs::create_dir(&outside).unwrap();
            let other = dir.path().join("other");
            fs::create_dir(&other).unwrap();
            fs::write(other.join("marker"), b"preserve").unwrap();
            let alias = dir.path().join("alias");
            symlink(&outside, &alias).unwrap();
            let source = dir.path().join("source.csv");
            fs::write(&source, b"id\none\n").unwrap();
            let stamp = SourceStamp::from_file(&File::open(&source).unwrap()).unwrap();
            let mut target =
                ExportTarget::new_combined(vec![(source.clone(), stamp)], alias.join("joined.csv"))
                    .unwrap();
            target.write_all(b"id\none\n").unwrap();
            if !cancel {
                fs::write(outside.join("joined.csv"), b"existing").unwrap();
            }
            fs::remove_file(&alias).unwrap();
            symlink(&other, &alias).unwrap();
            let result = target.publish(1, 7, &AtomicBool::new(cancel));
            if cancel {
                assert!(matches!(result, Ok(FilterExportOutcome::Cancelled)));
                assert!(fs::read_dir(&outside).unwrap().next().is_none());
            } else {
                assert!(matches!(result, Err(QuarryError::ExportDestinationExists)));
                assert_eq!(fs::read(outside.join("joined.csv")).unwrap(), b"existing");
                assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
            }
            assert_eq!(fs::read(other.join("marker")).unwrap(), b"preserve");
            assert_eq!(fs::read_dir(other).unwrap().count(), 1);
            assert_eq!(fs::read(source).unwrap(), b"id\none\n");
        }
    }

    fn duplicate_plan(dir: &Path, a: &[u8], b: &[u8], cfg: JoinConfig) -> JoinPlan {
        let mut sources = inputs(dir, a, b);
        for input in &mut sources {
            input.require_unique = false;
        }
        analyze_join(sources, dir.to_path_buf(), cfg)
            .unwrap()
            .wait()
            .unwrap()
            .unwrap()
    }

    #[test]
    fn all_duplicate_pairs_match_a_handwritten_oracle_and_require_acknowledgement() {
        let dir = tempfile::tempdir().unwrap();
        let a = b"id,left\nx,L1\nz,alone\nx,\"L,2\"\ny,Y1\ny,Y2\n";
        let b = b"id,right\ny,\"R\"\"Y\"\nx,R1\nx,R2\nnone,N\n";
        let p = duplicate_plan(dir.path(), a, b, config());
        assert_eq!(p.analysis.matching_rows, [4, 3]);
        assert_eq!(p.analysis.matching_pairs, 6);
        assert_eq!(p.analysis.duplicate_groups, [2, 1]);
        assert_eq!(p.analysis.many_to_many_groups, 1);
        assert_eq!(p.analysis.many_to_many_pairs, 4);
        assert_eq!(p.analysis.output_rows, 7);
        assert_eq!(p.analysis.unmatched_rows, [1, 1]);
        assert_eq!(p.analysis.preview[0], ["x", "L1", "x", "R1"]);
        assert_eq!(p.analysis.preview[1], ["x", "L1", "x", "R2"]);
        assert_eq!(p.analysis.preview[2], ["z", "alone", "", ""]);
        let output = dir.path().join("denied.csv");
        assert!(p.start_write(output.clone()).is_err());
        assert!(!output.exists());
        assert!(workspaces(dir.path()).is_empty());
        let p = duplicate_plan(dir.path(), a, b, config());
        let expected=b"id (Main),left,id (Additional),right\nx,L1,x,R1\nx,L1,x,R2\nz,alone,,\nx,\"L,2\",x,R1\nx,\"L,2\",x,R2\ny,Y1,y,\"R\"\"Y\"\ny,Y2,y,\"R\"\"Y\"\n";
        assert_eq!(p.analysis.output_bytes, expected.len() as u64);
        let allowance = p.analysis.additional_temporary_bytes
            + p.inputs.iter().map(|i| i.frame_bytes).sum::<u64>();
        let summary = p
            .start_write_acknowledged(output.clone(), true)
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(summary.data_rows, 7);
        assert_eq!(fs::read(output).unwrap(), expected);
        assert!(summary.peak_temporary_bytes <= allowance);
        assert!(summary.peak_group_buffer_bytes <= config().group_memory_bytes);
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    fn large_cartesian_analysis_counts_without_generating_the_full_output() {
        let dir = tempfile::tempdir().unwrap();
        let a = format!("id,value\n{}", "x,Main\n".repeat(5000));
        let b = format!("id,value\n{}", "x,Right\n".repeat(5000));
        let cfg = JoinConfig {
            memory_bytes: 4 * 1024 * 1024,
            run_memory_bytes: 32 * 1024,
            max_merge_fan_in: 4,
            ..config()
        };
        let p = duplicate_plan(dir.path(), a.as_bytes(), b.as_bytes(), cfg);
        assert_eq!(p.analysis.matching_rows, [5000, 5000]);
        assert_eq!(p.analysis.matching_pairs, 25_000_000);
        assert_eq!(p.analysis.output_rows, 25_000_000);
        assert_eq!(p.analysis.preview.len(), 3);
        assert_eq!(p.analysis.many_to_many_groups, 1);
        assert_eq!(
            p.analysis.output_bytes,
            output_header(&p.output, p.inputs[0].info.delimiter, cfg)
                .unwrap()
                .len() as u64
                + 25_000_000 * 15
        );
        let physical: u64 = fs::read_dir(p.workspace.as_ref().unwrap().path.clone())
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert!(
            physical < 2_000_000,
            "analysis should retain inputs, not the 375 MB Cartesian result"
        );
        p.release().unwrap();
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    fn a_duplicate_group_exceeding_the_block_budget_streams_exactly_and_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let a = format!(
            "id,value\n{}",
            (0..240)
                .map(|i| format!("x,{i:03}{}\n", "a".repeat(400)))
                .collect::<String>()
        );
        let b = format!(
            "id,value\n{}",
            (0..80).map(|i| format!("x,R{i:03}\n")).collect::<String>()
        );
        let cfg = JoinConfig {
            run_memory_bytes: 8 * 1024,
            group_memory_bytes: 512,
            ..config()
        };
        let p = duplicate_plan(dir.path(), a.as_bytes(), b.as_bytes(), cfg);
        assert_eq!(p.analysis.output_rows, 19200);
        let output = dir.path().join("expanded.csv");
        let summary = p
            .start_write_acknowledged(output.clone(), true)
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(summary.data_rows, 19200);
        assert!(summary.peak_group_buffer_bytes <= cfg.group_memory_bytes);
        // Verify every cross-product row independently, including block boundaries.
        {
            use std::io::BufRead;
            let mut reader = BufReader::new(File::open(&output).unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(
                line,
                "id (Main),value (Main),id (Additional),value (Additional)\n"
            );
            for main in 0..240 {
                for additional in 0..80 {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    assert_eq!(
                        line,
                        format!("x,{main:03}{},x,R{additional:03}\n", "a".repeat(400))
                    );
                }
            }
            line.clear();
            assert_eq!(reader.read_line(&mut line).unwrap(), 0);
        }
        let session = crate::Session::open(
            &output,
            crate::OpenOptions {
                rows: 3,
                header_mode: crate::HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(session.first_rows[1].fields[3], b"R000");
        assert_eq!(session.first_rows[2].fields[3], b"R001");
        assert!(workspaces(dir.path()).is_empty());
        let p = duplicate_plan(dir.path(), a.as_bytes(), b.as_bytes(), cfg);
        let output = dir.path().join("cancelled-expansion.csv");
        let job = p.start_write_acknowledged(output.clone(), true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while job.progress().records == 0 {
            assert!(!job.progress().done);
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        job.cancel();
        assert!(job.wait().unwrap().is_none());
        assert!(!output.exists());
        assert!(workspaces(dir.path()).is_empty());
        assert_eq!(fs::read(dir.path().join("main.csv")).unwrap(), a.as_bytes());
    }

    #[test]
    fn optional_uniqueness_checks_each_side_and_one_to_many_needs_no_ack() {
        let dir = tempfile::tempdir().unwrap();
        for side in 0..2 {
            let mut sources = inputs(dir.path(), b"id\nx\nx\n", b"id\nx\nx\n");
            for source in &mut sources {
                source.require_unique = false;
            }
            sources[side].require_unique = true;
            let job = analyze_join(sources, dir.path().to_path_buf(), config()).unwrap();
            assert!(job.wait().is_err());
            assert!(workspaces(dir.path()).is_empty());
        }
        let p = duplicate_plan(
            dir.path(),
            b"id\nx\n",
            b"id,value\nx,one\nx,two\n",
            config(),
        );
        assert_eq!(p.analysis.output_rows, 2);
        assert_eq!(p.analysis.many_to_many_groups, 0);
        let output = dir.path().join("one-to-many.csv");
        p.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(output).unwrap(),
            b"id (Main),id (Additional),value\nx,x,one\nx,x,two\n"
        );
    }

    #[test]
    fn expanded_group_count_and_bytes_overflow_are_rejected_before_generation() {
        let group = GroupRange {
            start: 0,
            end: 0,
            rows: 1u64 << 32,
            value_bytes: 1,
            max_value_bytes: 1,
        };
        assert!(matched_group_output(group, group, 4, 1024).is_err());
        let left = GroupRange {
            rows: 1,
            value_bytes: u64::MAX,
            ..group
        };
        let right = GroupRange {
            rows: 2,
            value_bytes: 2,
            ..group
        };
        assert!(matched_group_output(left, right, 4, 1024).is_err());
        let large = GroupRange {
            rows: 1,
            value_bytes: 1,
            max_value_bytes: 1024,
            ..group
        };
        assert!(matched_group_output(large, large, 4, 1024).is_err());
    }
    // Deliberately separate CSV serialization and nested-loop ordering oracle.
    fn oracle_csv(rows: &[Vec<String>]) -> Vec<u8> {
        let mut result = String::new();
        for row in rows {
            for (i, field) in row.iter().enumerate() {
                if i > 0 {
                    result.push(',');
                }
                if field.contains([',', '"', '\r', '\n'])
                    || (i == 0 && field.starts_with('\u{feff}'))
                {
                    result.push('"');
                    result.push_str(&field.replace('"', "\"\""));
                    result.push('"');
                } else {
                    result.push_str(field);
                }
            }
            result.push('\n');
        }
        result.into_bytes()
    }

    fn oracle_matches(a: &[String], b: &[String]) -> bool {
        !a[0].is_empty() && a[0] == b[0]
    }

    fn oracle_rows(mode: JoinMode, a: &[Vec<String>], b: &[Vec<String>]) -> Vec<Vec<String>> {
        let pair = |a: Option<&Vec<String>>, b: Option<&Vec<String>>| -> Vec<String> {
            (0..2)
                .map(|i| a.map_or_else(String::new, |row| row[i].clone()))
                .chain((0..2).map(|i| b.map_or_else(String::new, |row| row[i].clone())))
                .collect()
        };
        let mut rows = Vec::new();
        if mode == JoinMode::Right {
            for right in b {
                let mut found = false;
                for left in a {
                    if oracle_matches(left, right) {
                        found = true;
                        rows.push(pair(Some(left), Some(right)));
                    }
                }
                if !found {
                    rows.push(pair(None, Some(right)));
                }
            }
        } else {
            let mut seen = vec![false; b.len()];
            for left in a {
                let mut found = false;
                for (i, right) in b.iter().enumerate() {
                    if oracle_matches(left, right) {
                        found = true;
                        seen[i] = true;
                        rows.push(pair(Some(left), Some(right)));
                    }
                }
                if !found && mode != JoinMode::Inner {
                    rows.push(pair(Some(left), None));
                }
            }
            if mode == JoinMode::Full {
                for (i, right) in b.iter().enumerate() {
                    if !seen[i] {
                        rows.push(pair(None, Some(right)));
                    }
                }
            }
        }
        rows
    }

    #[test]
    fn every_join_mode_matches_an_independent_oracle_through_spills_and_reopen() {
        let rows = |data: &[(&str, &str)]| -> Vec<Vec<String>> {
            data.iter()
                .map(|(key, value)| vec![key.to_string(), value.to_string()])
                .collect()
        };
        let cases = [
            (
                rows(&[
                    ("x", "A0"),
                    ("z", "solo"),
                    ("x", "A1"),
                    ("", "empty"),
                    (" ", "space"),
                    ("01", "zero"),
                    ("a", "lower"),
                    ("À", "unicode"),
                    ("B", "quote,\"line\nnext"),
                ]),
                rows(&[
                    ("x", "R0"),
                    ("u", "extra"),
                    ("x", "R1"),
                    ("", "empty-other"),
                    (" ", "space-right"),
                    ("1", "one"),
                    ("A", "upper"),
                    ("A\u{300}", "decomposed"),
                    ("B", "match"),
                ]),
            ),
            (vec![], rows(&[("x", "one"), ("", "empty"), ("x", "two")])),
            (rows(&[("x", "one"), ("", "empty"), ("x", "two")]), vec![]),
            (vec![], vec![]),
            (rows(&[("a", "left")]), rows(&[("b", "right")])),
            (
                rows(&[("x", "L1"), ("x", "L2")]),
                rows(&[("x", "R1"), ("y", "R2"), ("y", "R3")]),
            ),
        ];
        for (a, b) in cases {
            for mode in [
                JoinMode::Left,
                JoinMode::Inner,
                JoinMode::Right,
                JoinMode::Full,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let header = vec!["key".to_string(), "value".to_string()];
                let mut source_a = vec![header.clone()];
                source_a.extend(a.clone());
                let mut source_b = vec![header];
                source_b.extend(b.clone());
                let source_a = oracle_csv(&source_a);
                let source_b = oracle_csv(&source_b);
                let cfg = JoinConfig {
                    mode,
                    run_memory_bytes: 128,
                    group_memory_bytes: 64,
                    ..config()
                };
                let p = duplicate_plan(dir.path(), &source_a, &source_b, cfg);
                let wanted = oracle_rows(mode, &a, &b);
                let mut expected = vec![vec![
                    "key (Main)".into(),
                    "value (Main)".into(),
                    "key (Additional)".into(),
                    "value (Additional)".into(),
                ]];
                expected.extend(wanted.clone());
                let expected = oracle_csv(&expected);
                let pairs = a
                    .iter()
                    .flat_map(|left| b.iter().map(move |right| oracle_matches(left, right)))
                    .filter(|v| *v)
                    .count() as u64;
                let main_matches = a
                    .iter()
                    .filter(|left| b.iter().any(|right| oracle_matches(left, right)))
                    .count() as u64;
                let additional_matches = b
                    .iter()
                    .filter(|right| a.iter().any(|left| oracle_matches(left, right)))
                    .count() as u64;
                assert_eq!(p.analysis.matching_pairs, pairs, "{mode:?}");
                assert_eq!(p.analysis.matching_rows, [main_matches, additional_matches]);
                assert_eq!(
                    p.analysis.unmatched_rows,
                    [
                        a.len() as u64 - main_matches,
                        b.len() as u64 - additional_matches
                    ]
                );
                assert_eq!(p.analysis.output_rows, wanted.len() as u64, "{mode:?}");
                assert_eq!(p.analysis.output_bytes, expected.len() as u64, "{mode:?}");
                assert_eq!(
                    p.analysis.preview,
                    wanted
                        .into_iter()
                        .take(cfg.preview_rows)
                        .collect::<Vec<_>>(),
                    "{mode:?}"
                );
                let output = dir.path().join("joined.csv");
                let summary = p
                    .start_write_acknowledged(output.clone(), true)
                    .unwrap()
                    .wait()
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    summary.data_rows + 1,
                    crate::Session::open(
                        &output,
                        crate::OpenOptions {
                            rows: 100,
                            ..Default::default()
                        }
                    )
                    .unwrap()
                    .first_rows
                    .len() as u64
                );
                assert_eq!(fs::read(&output).unwrap(), expected, "{mode:?}");
                assert_eq!(fs::read(dir.path().join("main.csv")).unwrap(), source_a);
                assert_eq!(
                    fs::read(dir.path().join("additional.csv")).unwrap(),
                    source_b
                );
                assert!(workspaces(dir.path()).is_empty());
            }
        }
    }

    #[test]
    fn output_order_validation_rejects_wrong_sides_and_out_of_range_ordinals() {
        for mode in [
            JoinMode::Left,
            JoinMode::Inner,
            JoinMode::Right,
            JoinMode::Full,
        ] {
            for (main, additional, valid) in [
                (Some(1), Some(2), true),
                (Some(2), Some(1), false),
                (Some(0), Some(3), false),
                (None, None, false),
                (
                    Some(0),
                    None,
                    matches!(mode, JoinMode::Left | JoinMode::Full),
                ),
                (
                    None,
                    Some(0),
                    matches!(mode, JoinMode::Right | JoinMode::Full),
                ),
            ] {
                let order = pair_order(mode, main, additional);
                assert_eq!(
                    valid_pair_order(&pair_key(order), order.0, mode, [2, 3]),
                    valid,
                    "{mode:?}"
                );
                assert!(!valid_pair_order(&pair_key(order), 42, mode, [2, 3]));
            }
        }
    }
    fn tuple_matches(a: &[String], b: &[String], ascii: bool) -> bool {
        [(0, 1), (1, 0)].iter().all(|&(left, right)| {
            let Some(a) = a.get(left).filter(|value| !value.is_empty()) else {
                return false;
            };
            let Some(b) = b.get(right).filter(|value| !value.is_empty()) else {
                return false;
            };
            if ascii {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        })
    }

    fn tuple_oracle(
        mode: JoinMode,
        a: &[Vec<String>],
        b: &[Vec<String>],
        ascii: bool,
    ) -> Vec<Vec<String>> {
        let pair = |a: Option<&Vec<String>>, b: Option<&Vec<String>>| {
            (0..3)
                .map(|i| a.and_then(|row| row.get(i)).cloned().unwrap_or_default())
                .chain((0..3).map(|i| b.and_then(|row| row.get(i)).cloned().unwrap_or_default()))
                .collect::<Vec<_>>()
        };
        let mut result = Vec::new();
        if mode == JoinMode::Right {
            for right in b {
                let mut found = false;
                for left in a {
                    if tuple_matches(left, right, ascii) {
                        found = true;
                        result.push(pair(Some(left), Some(right)));
                    }
                }
                if !found {
                    result.push(pair(None, Some(right)));
                }
            }
        } else {
            let mut used = vec![false; b.len()];
            for left in a {
                let mut found = false;
                for (i, right) in b.iter().enumerate() {
                    if tuple_matches(left, right, ascii) {
                        found = true;
                        used[i] = true;
                        result.push(pair(Some(left), Some(right)));
                    }
                }
                if !found && mode != JoinMode::Inner {
                    result.push(pair(Some(left), None));
                }
            }
            if mode == JoinMode::Full {
                for (i, right) in b.iter().enumerate() {
                    if !used[i] {
                        result.push(pair(None, Some(right)));
                    }
                }
            }
        }
        result
    }

    #[test]
    fn composite_keys_and_ascii_case_match_an_independent_oracle_in_every_mode() {
        let row = |a: &str, b: &str, c: &str| vec![a.to_owned(), b.to_owned(), c.to_owned()];
        let a = vec![
            row("ab", "c", "boundary one"),
            row("a", "bc", "boundary two"),
            row("x\0y", "z", "nul one"),
            row("x", "y\0z", "nul two"),
            row("Ab", "C", "ASCII"),
            row("é", "1", "lower"),
            row("É", "1", "upper"),
            row("é", "01", "leading zero"),
            row("e\u{301}", "1", "decomposed"),
            row(" ", "1", "space"),
            row("", "q", "empty first"),
            row("x", "", "empty second"),
            vec!["missing".into()],
            row("dup", "D", "first"),
            row("dup", "D", "second"),
        ];
        let b = vec![
            row("c", "ab", "boundary one"),
            row("bc", "a", "boundary two"),
            row("z", "x\0y", "nul one"),
            row("y\0z", "x", "nul two"),
            row("c", "aB", "ASCII"),
            row("1", "é", "lower"),
            row("1", "É", "upper"),
            row("01", "é", "leading zero"),
            row("1", "e\u{301}", "decomposed"),
            row("1", " ", "space"),
            row("q", "", "empty first"),
            row("", "x", "empty second"),
            vec!["missing".into()],
            row("D", "dup", "first"),
            row("d", "DUP", "second"),
            row("1", "other", "unmatched"),
        ];
        for ascii in [false, true] {
            for mode in [
                JoinMode::Left,
                JoinMode::Inner,
                JoinMode::Right,
                JoinMode::Full,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let mut left = vec![row("region", "account", "left")];
                left.extend(a.clone());
                let mut right = vec![row("account2", "region2", "right")];
                right.extend(b.clone());
                let left = oracle_csv(&left);
                let right = oracle_csv(&right);
                let mut sources = inputs(dir.path(), &left, &right);
                sources[0].key_columns = vec![0, 1];
                sources[1].key_columns = vec![1, 0];
                for source in &mut sources {
                    source.require_unique = false;
                }
                let cfg = JoinConfig {
                    mode,
                    case: if ascii {
                        JoinCase::AsciiInsensitive
                    } else {
                        JoinCase::Sensitive
                    },
                    ..config()
                };
                let p = analyze_join(sources, dir.path().to_path_buf(), cfg)
                    .unwrap()
                    .wait()
                    .unwrap()
                    .unwrap();
                let wanted = tuple_oracle(mode, &a, &b, ascii);
                let pairs = a
                    .iter()
                    .flat_map(|a| b.iter().map(move |b| tuple_matches(a, b, ascii)))
                    .filter(|same| *same)
                    .count();
                assert_eq!(p.analysis.matching_pairs, pairs as u64);
                assert_eq!(p.analysis.empty_key_rows, [3, 3]);
                assert_eq!(p.analysis.output_rows, wanted.len() as u64);
                assert_eq!(
                    p.analysis.preview,
                    wanted
                        .iter()
                        .take(cfg.preview_rows)
                        .cloned()
                        .collect::<Vec<_>>()
                );
                let mut rows = vec![vec![
                    "region".into(),
                    "account".into(),
                    "left".into(),
                    "account2".into(),
                    "region2".into(),
                    "right".into(),
                ]];
                rows.extend(wanted.clone());
                let expected = oracle_csv(&rows);
                assert_eq!(p.analysis.output_bytes, expected.len() as u64);
                let output = dir.path().join("tuple.csv");
                p.start_write_acknowledged(output.clone(), true)
                    .unwrap()
                    .wait()
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    fs::read(&output).unwrap(),
                    expected,
                    "{mode:?} ASCII={ascii}"
                );
                let reopened = crate::Session::open(
                    &output,
                    crate::OpenOptions {
                        rows: 100,
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(reopened.first_rows.len(), wanted.len() + 1);
                assert_eq!(fs::read(dir.path().join("main.csv")).unwrap(), left);
                assert_eq!(fs::read(dir.path().join("additional.csv")).unwrap(), right);
                assert!(workspaces(dir.path()).is_empty());
            }
        }
    }

    #[test]
    fn tuple_framing_has_no_boundary_or_nul_collisions_and_is_bounded() {
        let fields = |parts: &[&str]| {
            parts
                .iter()
                .map(|s| Cow::Owned(s.as_bytes().to_vec()))
                .collect::<Vec<_>>()
        };
        let cfg = config();
        let a = key(&fields(&["ab", "c"]), &[0, 1], 0, cfg).unwrap();
        let b = key(&fields(&["a", "bc"]), &[0, 1], 0, cfg).unwrap();
        let c = key(&fields(&["a\0b", "c"]), &[0, 1], 0, cfg).unwrap();
        let d = key(&fields(&["a", "b\0c"]), &[0, 1], 0, cfg).unwrap();
        assert_ne!(a, b);
        assert_ne!(c, d);
        assert_eq!(key_label(&c).unwrap(), "(\"a\\0b\", \"c\")");
        assert!(!key_matches(
            &a,
            &fields(&["a", "bc"]),
            &[0, 1],
            0,
            JoinCase::Sensitive
        ));
        let mut trailing = a.clone();
        trailing.push(0);
        assert!(!key_matches(
            &trailing,
            &fields(&["ab", "c"]),
            &[0, 1],
            0,
            JoinCase::Sensitive
        ));
        let long = key(&fields(&[&"é".repeat(300)]), &[0], 0, cfg).unwrap();
        assert!(key_label(&long).unwrap().len() <= 133);
        let cfg = JoinConfig {
            max_record_bytes: 32,
            ..cfg
        };
        assert!(key(&fields(&[&"x".repeat(20)]), &[0; 16], 0, cfg).is_err());
    }

    #[test]
    fn pair_count_column_bounds_and_sixteen_component_matching_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        for (a, b) in [
            (vec![], vec![]),
            (vec![0; 17], vec![0; 17]),
            (vec![0], vec![0, 0]),
        ] {
            let mut sources = inputs(dir.path(), b"id\nx\n", b"id\nx\n");
            sources[0].key_columns = a;
            sources[1].key_columns = b;
            assert!(analyze_join(sources, dir.path().to_path_buf(), config()).is_err());
            assert!(workspaces(dir.path()).is_empty());
        }
        let mut sources = inputs(dir.path(), b"id\nx\n", b"id\nx\n");
        sources[0].key_columns = vec![1];
        assert!(
            analyze_join(sources, dir.path().to_path_buf(), config())
                .unwrap()
                .wait()
                .is_err()
        );
        let mut left = vec![(0..16).map(|i| format!("left{i}")).collect::<Vec<_>>()];
        left.push((0..16).map(|i| format!("value{i}")).collect());
        let mut right = vec![(0..16).map(|i| format!("right{i}")).collect::<Vec<_>>()];
        right.push(left[1].iter().rev().cloned().collect());
        let mut unmatched = right[1].clone();
        unmatched[0] = "different".into();
        right.push(unmatched);
        let mut sources = inputs(dir.path(), &oracle_csv(&left), &oracle_csv(&right));
        sources[0].key_columns = (0..16).collect();
        sources[1].key_columns = (0..16).rev().collect();
        let cfg = JoinConfig {
            max_columns: 32,
            ..config()
        };
        let p = analyze_join(sources, dir.path().to_path_buf(), cfg)
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(p.analysis.matching_pairs, 1);
        assert_eq!(p.analysis.unmatched_rows, [0, 1]);
        let expected = oracle_csv(&[
            left[0].iter().chain(&right[0]).cloned().collect(),
            left[1].iter().chain(&right[1]).cloned().collect(),
        ]);
        assert_eq!(p.analysis.output_bytes, expected.len() as u64);
        let output = dir.path().join("sixteen-pairs.csv");
        p.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(output).unwrap(), expected);
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    fn ascii_uniqueness_and_duplicate_analysis_use_the_same_tuple_comparison() {
        let dir = tempfile::tempdir().unwrap();
        for case in [JoinCase::Sensitive, JoinCase::AsciiInsensitive] {
            let sources = inputs(dir.path(), b"id,part\nA,x\na,x\n", b"id,part\na,x\n");
            let cfg = JoinConfig { case, ..config() };
            let result = analyze_join(sources, dir.path().to_path_buf(), cfg)
                .unwrap()
                .wait();
            if case == JoinCase::Sensitive {
                result.unwrap().unwrap().release().unwrap();
            } else {
                assert!(result.is_err());
            }
        }
        let mut sources = inputs(dir.path(), b"id,part\nA,x\na,x\n", b"id,part\na,x\nA,X\n");
        for input in &mut sources {
            input.key_columns = vec![0, 1];
            input.require_unique = false;
        }
        let p = analyze_join(
            sources,
            dir.path().to_path_buf(),
            JoinConfig {
                case: JoinCase::AsciiInsensitive,
                ..config()
            },
        )
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
        assert_eq!(p.analysis.duplicate_groups, [1, 1]);
        assert_eq!(p.analysis.many_to_many_groups, 1);
        assert_eq!(p.analysis.matching_pairs, 4);
        let output = dir.path().join("unacknowledged.csv");
        assert!(p.start_write(output.clone()).is_err());
        assert!(!output.exists());
        assert!(workspaces(dir.path()).is_empty());
    }
    fn output_column(index: usize, name: Option<&str>) -> JoinOutputColumn {
        JoinOutputColumn {
            source: if index < 2 {
                JoinSource::Main
            } else {
                JoinSource::Additional
            },
            column: index % 2,
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn projected_joins_match_independent_values_and_order_with_or_without_headers() {
        let row = |a: &str, b: &str| vec![a.to_owned(), b.to_owned()];
        let a = vec![
            row("x", "Left,\" one"),
            row("z", "alone\nline"),
            row("", "empty"),
            row("x", "L2"),
        ];
        let b = vec![
            row("x", "\u{feff}marker"),
            row("y", "extra"),
            row("x", "R2"),
            row("", "emptyR"),
        ];
        let mut left = vec![row("id", "value")];
        left.extend(a.clone());
        let mut right = vec![row("id", "value")];
        right.extend(b.clone());
        let source_left = oracle_csv(&left);
        let source_right = oracle_csv(&right);
        for mode in [
            JoinMode::Left,
            JoinMode::Inner,
            JoinMode::Right,
            JoinMode::Full,
        ] {
            for include_header in [false, true] {
                for projection in [vec![3, 1, 2, 0], vec![3, 1], vec![3], vec![1]] {
                    let dir = tempfile::tempdir().unwrap();
                    let mut sources = inputs(dir.path(), &source_left, &source_right);
                    for source in &mut sources {
                        source.require_unique = false;
                    }
                    let names = projection
                        .iter()
                        .map(|i| format!("selected {i}"))
                        .collect::<Vec<_>>();
                    let columns = projection
                        .iter()
                        .zip(&names)
                        .map(|(&i, name)| output_column(i, Some(name)))
                        .collect();
                    let cfg = JoinConfig { mode, ..config() };
                    let p = analyze_join_with_output(
                        sources,
                        dir.path().to_path_buf(),
                        cfg,
                        JoinOutputOptions {
                            known_widths: None,
                            columns: Some(columns),
                            include_header: Some(include_header),
                        },
                    )
                    .unwrap()
                    .wait()
                    .unwrap()
                    .unwrap();
                    let wanted = oracle_rows(mode, &a, &b)
                        .into_iter()
                        .map(|row| {
                            projection
                                .iter()
                                .map(|&i| row[i].clone())
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(
                        p.analysis.preview,
                        wanted
                            .iter()
                            .take(cfg.preview_rows)
                            .cloned()
                            .collect::<Vec<_>>()
                    );
                    assert_eq!(p.analysis.output_headers.as_ref(), names);
                    let mut rows = Vec::new();
                    if include_header {
                        rows.push(names);
                    }
                    rows.extend(wanted.clone());
                    let expected = oracle_csv(&rows);
                    assert_eq!(p.analysis.output_bytes, expected.len() as u64);
                    let output = dir.path().join("projected.csv");
                    let summary = p
                        .start_write_acknowledged(output.clone(), true)
                        .unwrap()
                        .wait()
                        .unwrap()
                        .unwrap();
                    assert_eq!(summary.include_header, include_header);
                    assert_eq!(
                        fs::read(&output).unwrap(),
                        expected,
                        "{mode:?}, header={include_header}, projection={projection:?}"
                    );
                    let reopened = crate::Session::open(
                        &output,
                        crate::OpenOptions {
                            rows: 100,
                            header_mode: if include_header {
                                crate::HeaderMode::FirstRow
                            } else {
                                crate::HeaderMode::NoHeader
                            },
                            ..Default::default()
                        },
                    )
                    .unwrap();
                    let data = if include_header {
                        &reopened.first_rows[1..]
                    } else {
                        &reopened.first_rows[..]
                    };
                    assert_eq!(data.len(), wanted.len());
                    for (row, expected) in data.iter().zip(&wanted) {
                        assert_eq!(
                            row.fields,
                            expected
                                .iter()
                                .map(|s| s.as_bytes().to_vec())
                                .collect::<Vec<_>>()
                        );
                    }
                    assert_eq!(fs::read(dir.path().join("main.csv")).unwrap(), source_left);
                    assert_eq!(
                        fs::read(dir.path().join("additional.csv")).unwrap(),
                        source_right
                    );
                    assert!(workspaces(dir.path()).is_empty());
                }
            }
        }
    }

    #[test]
    fn output_names_are_disambiguated_but_explicit_duplicates_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let make = || {
            inputs(
                dir.path(),
                b"id,,dup,dup\nx,a,b,c\n",
                b"id,dup,solo\nx,d,e\n",
            )
        };
        let p = analyze_join(make(), dir.path().to_path_buf(), config())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            p.analysis.output_headers.as_ref(),
            [
                "id (Main)",
                "Main column 2",
                "dup (Main)",
                "dup (Main) 2",
                "id (Additional)",
                "dup (Additional)",
                "solo"
            ]
        );
        assert!(!p.analysis.duplicate_headers);
        p.release().unwrap();
        let columns = vec![
            JoinOutputColumn {
                source: JoinSource::Main,
                column: 0,
                name: Some("same".into()),
            },
            JoinOutputColumn {
                source: JoinSource::Additional,
                column: 0,
                name: Some("same".into()),
            },
            JoinOutputColumn {
                source: JoinSource::Main,
                column: 2,
                name: None,
            },
        ];
        let p = analyze_join_with_output(
            make(),
            dir.path().to_path_buf(),
            config(),
            JoinOutputOptions {
                known_widths: None,
                columns: Some(columns),
                include_header: Some(true),
            },
        )
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
        assert_eq!(p.analysis.output_headers.as_ref(), ["same", "same", "dup"]);
        assert!(p.analysis.duplicate_headers);
        let output = dir.path().join("duplicate-headings.csv");
        p.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(output).unwrap(), b"same,same,dup\nx,x,b\n");
    }

    #[test]
    fn projection_limits_apply_to_selected_columns_and_serialized_output() {
        let dir = tempfile::tempdir().unwrap();
        for columns in [vec![], vec![output_column(0, None); 3]] {
            let sources = inputs(dir.path(), b"id,x\na,b\n", b"id,y\na,c\n");
            let result = analyze_join_with_output(
                sources,
                dir.path().to_path_buf(),
                JoinConfig {
                    max_columns: 2,
                    ..config()
                },
                JoinOutputOptions {
                    known_widths: None,
                    columns: Some(columns),
                    include_header: None,
                },
            );
            assert!(result.is_err());
            assert!(workspaces(dir.path()).is_empty());
        }
        let sources = inputs(dir.path(), b"id,x\na,b\n", b"id,y\na,c\n");
        let p = analyze_join_with_output(
            sources,
            dir.path().to_path_buf(),
            JoinConfig {
                max_columns: 2,
                ..config()
            },
            JoinOutputOptions {
                known_widths: None,
                columns: Some(vec![output_column(1, None), output_column(3, None)]),
                include_header: None,
            },
        )
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
        assert_eq!(p.analysis.output_rows, 1);
        p.release().unwrap();
        let sources = inputs(dir.path(), b"id,x\na,b\n", b"id,y\na,c\n");
        let result = analyze_join_with_output(
            sources,
            dir.path().to_path_buf(),
            config(),
            JoinOutputOptions {
                known_widths: None,
                columns: Some(vec![JoinOutputColumn {
                    source: JoinSource::Main,
                    column: 2,
                    name: None,
                }]),
                include_header: None,
            },
        );
        assert!(result.unwrap().wait().is_err());
        let a = format!("id,x\na,{}\n", "v".repeat(40));
        let b = format!("id,y\na,{}\n", "w".repeat(40));
        let cfg = JoinConfig {
            max_record_bytes: 64,
            ..config()
        };
        assert!(
            analyze_join(
                inputs(dir.path(), a.as_bytes(), b.as_bytes()),
                dir.path().to_path_buf(),
                cfg
            )
            .unwrap()
            .wait()
            .is_err()
        );
        let p = analyze_join_with_output(
            inputs(dir.path(), a.as_bytes(), b.as_bytes()),
            dir.path().to_path_buf(),
            cfg,
            JoinOutputOptions {
                known_widths: None,
                columns: Some(vec![output_column(1, None)]),
                include_header: None,
            },
        )
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
        p.release().unwrap();
        let columns = vec![output_column(1, Some(&"\"".repeat(65)))];
        assert!(
            analyze_join_with_output(
                inputs(dir.path(), b"id,x\na,b\n", b"id,y\na,c\n"),
                dir.path().to_path_buf(),
                JoinConfig {
                    max_record_bytes: 128,
                    ..config()
                },
                JoinOutputOptions {
                    known_widths: None,
                    columns: Some(columns),
                    include_header: Some(true)
                }
            )
            .unwrap()
            .wait()
            .is_err()
        );
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    fn zero_row_headerless_output_is_a_valid_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = analyze_join_with_output(
            inputs(dir.path(), b"id\n", b"id\n"),
            dir.path().to_path_buf(),
            config(),
            JoinOutputOptions {
                known_widths: None,
                columns: None,
                include_header: Some(false),
            },
        )
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
        assert_eq!(p.analysis.output_bytes, 0);
        let output = dir.path().join("empty.csv");
        let summary = p
            .start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert!(!summary.include_header);
        assert_eq!(fs::read(output).unwrap(), b"");
        assert!(workspaces(dir.path()).is_empty());
    }
    /// Independent fixture encoder: no Quarry decoding/serialization functions.
    fn source_bytes(text: &str, encoding: Encoding) -> Vec<u8> {
        match encoding {
            Encoding::Utf8 => text.as_bytes().to_vec(),
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let mut bytes = if encoding == Encoding::Utf16Le {
                    vec![0xff, 0xfe]
                } else {
                    vec![0xfe, 0xff]
                };
                for unit in text.encode_utf16() {
                    bytes.extend_from_slice(&if encoding == Encoding::Utf16Le {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    });
                }
                bytes
            }
            Encoding::Latin1 | Encoding::Windows1252 => text
                .chars()
                .map(|ch| {
                    if ch == '€' && encoding == Encoding::Windows1252 {
                        0x80
                    } else {
                        u8::try_from(u32::from(ch)).expect("single-byte fixture")
                    }
                })
                .collect(),
        }
    }

    fn delimited_fixture(rows: &[Vec<String>], delimiter: u8) -> String {
        let mut result = String::new();
        for row in rows {
            for (index, value) in row.iter().enumerate() {
                if index != 0 {
                    result.push(char::from(delimiter));
                }
                let quote = value
                    .bytes()
                    .any(|b| [delimiter, b'"', b'\r', b'\n'].contains(&b))
                    || (index == 0 && value.starts_with('\u{feff}'));
                if quote {
                    result.push('"');
                }
                result.push_str(&value.replace('"', "\"\""));
                if quote {
                    result.push('"');
                }
            }
            result.push('\n');
        }
        result
    }

    #[test]
    fn mixed_encodings_delimiters_and_header_modes_preserve_exact_values() {
        let row = |a: &str, b: &str| vec![a.to_owned(), b.to_owned()];
        for ae in Encoding::ALL {
            for be in Encoding::ALL {
                for ah in [false, true] {
                    for bh in [false, true] {
                        let dir = tempfile::tempdir().unwrap();
                        let a = vec![row("é", "Left,; \"quoted\"\r\nline"), row("z", "alone")];
                        let b = vec![row("é", "café"), row("q", "extra")];
                        let mut ar = if ah { vec![row("id", "left")] } else { vec![] };
                        ar.extend(a.clone());
                        let mut br = if bh {
                            vec![row("key", "right")]
                        } else {
                            vec![]
                        };
                        br.extend(b.clone());
                        let ab = source_bytes(&delimited_fixture(&ar, b';'), ae);
                        let bb = source_bytes(&delimited_fixture(&br, b'\t'), be);
                        let mut args = inputs(dir.path(), &ab, &bb);
                        args[0].delimiter = Some(b';');
                        args[1].delimiter = Some(b'\t');
                        args[0].encoding = Some(ae);
                        args[1].encoding = Some(be);
                        args[0].has_header = ah;
                        args[1].has_header = bh;
                        let p = analyze_join(
                            args,
                            dir.path().to_path_buf(),
                            JoinConfig {
                                mode: JoinMode::Full,
                                ..config()
                            },
                        )
                        .unwrap()
                        .wait()
                        .unwrap()
                        .unwrap();
                        assert_eq!(p.analysis.input_rows, [2, 2]);
                        assert_eq!([p.inputs()[0].encoding, p.inputs()[1].encoding], [ae, be]);
                        let mut wanted = Vec::new();
                        if ah || bh {
                            wanted.push(vec![
                                if ah { "id" } else { "Main column 1" }.into(),
                                if ah { "left" } else { "Main column 2" }.into(),
                                if bh { "key" } else { "Additional column 1" }.into(),
                                if bh { "right" } else { "Additional column 2" }.into(),
                            ]);
                        }
                        wanted.extend(oracle_rows(JoinMode::Full, &a, &b));
                        let expected = delimited_fixture(&wanted, b';').into_bytes();
                        assert_eq!(p.analysis.output_bytes, expected.len() as u64);
                        let output = dir.path().join("mixed.csv");
                        p.start_write(output.clone())
                            .unwrap()
                            .wait()
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            fs::read(output).unwrap(),
                            expected,
                            "{ae:?}/{be:?},headers={ah}/{bh}"
                        );
                        assert_eq!(fs::read(dir.path().join("main.csv")).unwrap(), ab);
                        assert_eq!(fs::read(dir.path().join("additional.csv")).unwrap(), bb);
                        assert!(workspaces(dir.path()).is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn late_columns_are_included_reviewed_and_use_stable_source_positions() {
        let dir = tempfile::tempdir().unwrap();
        let make = || {
            let mut args = inputs(dir.path(), b"a\nb,x\n", b"id\nx\n");
            args[0].has_header = false;
            args[0].key_columns = vec![1];
            analyze_join_with_output(
                args,
                dir.path().to_path_buf(),
                config(),
                JoinOutputOptions {
                    known_widths: Some([1, 1]),
                    columns: Some(vec![
                        JoinOutputColumn {
                            source: JoinSource::Main,
                            column: 0,
                            name: None,
                        },
                        JoinOutputColumn {
                            source: JoinSource::Additional,
                            column: 0,
                            name: None,
                        },
                    ]),
                    include_header: None,
                },
            )
            .unwrap()
        };
        let job = make();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !job.progress().done {
            assert!(
                std::time::Instant::now() < deadline,
                "schema analysis timed out"
            );
            thread::sleep(std::time::Duration::from_millis(1));
        }
        let headers = job.take_discovered_headers().unwrap();
        assert_eq!(headers[0].columns.len(), 2);
        assert!(!headers[0].has_header);
        let p = job.wait().unwrap().unwrap();
        assert_eq!(p.analysis.new_columns, [1, 0]);
        assert_eq!(p.analysis.empty_key_rows, [1, 0]);
        assert_eq!(p.analysis.preview, [vec!["a", "", ""], vec!["b", "x", "x"]]);
        let output = dir.path().join("late.csv");
        assert!(
            p.start_write(output.clone()).is_err(),
            "new layout needs explicit review"
        );
        assert!(!output.exists());
        let p = make().wait().unwrap().unwrap();
        p.start_write_reviewed(output.clone(), false, true)
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(output).unwrap(),
            b"Main column 1,id,Main column 2\na,,\nb,x,x\n"
        );
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    fn discovered_schema_is_available_when_new_columns_exceed_output_limit() {
        let dir = tempfile::tempdir().unwrap();
        let args = inputs(dir.path(), b"id\nx,late\n", b"id\nx,more\n");
        let job = analyze_join_with_output(
            args,
            dir.path().to_path_buf(),
            JoinConfig {
                max_columns: 2,
                ..config()
            },
            JoinOutputOptions {
                known_widths: Some([1, 1]),
                columns: Some(vec![
                    JoinOutputColumn {
                        source: JoinSource::Main,
                        column: 0,
                        name: None,
                    },
                    JoinOutputColumn {
                        source: JoinSource::Additional,
                        column: 0,
                        name: None,
                    },
                ]),
                include_header: None,
            },
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !job.progress().done {
            assert!(
                std::time::Instant::now() < deadline,
                "schema analysis timed out"
            );
            thread::sleep(std::time::Duration::from_millis(1));
        }
        let headers = job.take_discovered_headers().unwrap();
        assert_eq!([headers[0].columns.len(), headers[1].columns.len()], [2, 2]);
        assert!(job.wait().is_err());
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
    fn format_probe_decodes_labels_and_rejects_malformed_unselected_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf16.tsv");
        fs::write(
            &path,
            source_bytes("clé\tname\nx\tvalue\n", Encoding::Utf16Be),
        )
        .unwrap();
        let h = read_join_header_with_format(path, JoinFormat::default())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(h.encoding, Encoding::Utf16Be);
        assert_eq!(h.delimiter, b'\t');
        assert_eq!(h.columns, ["clé", "name"]);
        let mut args = inputs(dir.path(), b"id,other\nx,\x81\n", b"id\nx\n");
        args[0].encoding = Some(Encoding::Windows1252);
        assert!(
            analyze_join(args, dir.path().to_path_buf(), config())
                .unwrap()
                .wait()
                .is_err()
        );
        let mut args = inputs(dir.path(), b"\xff\xfe\0\0", b"id\nx\n");
        args[0].encoding = None;
        assert!(
            analyze_join(args, dir.path().to_path_buf(), config())
                .unwrap()
                .wait()
                .is_err()
        );
        assert!(workspaces(dir.path()).is_empty());
    }
    #[test]
    fn malformed_excluded_values_keep_source_and_record_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let bad_utf16 = {
            let mut b = source_bytes("id,other\nx,", Encoding::Utf16Le);
            b.extend_from_slice(&[0x00, 0xd8, 0x0a, 0x00]);
            b
        };
        let mut odd_utf16 = source_bytes("id,other\nx,", Encoding::Utf16Le);
        odd_utf16.push(0);
        for (encoding, bytes) in [
            (Encoding::Utf8, b"id,other\nx,\xff\n".to_vec()),
            (Encoding::Windows1252, b"id,other\nx,\x81\n".to_vec()),
            (Encoding::Utf16Le, bad_utf16),
            (Encoding::Utf16Le, odd_utf16),
        ] {
            let mut args = inputs(dir.path(), &bytes, b"id\nx\n");
            args[0].encoding = Some(encoding);
            let result = analyze_join_with_output(
                args,
                dir.path().to_path_buf(),
                config(),
                JoinOutputOptions {
                    known_widths: None,
                    columns: Some(vec![JoinOutputColumn {
                        source: JoinSource::Main,
                        column: 0,
                        name: None,
                    }]),
                    include_header: None,
                },
            )
            .unwrap()
            .wait();
            let err = result
                .err()
                .expect("excluded malformed text must block analysis");
            assert!(
                matches!(err,QuarryError::JoinFile{path:Some(ref p),record:Some(2),..}
                if p == &dir.path().join("main.csv")),
                "{err:?}"
            );
            assert_eq!(fs::read(dir.path().join("main.csv")).unwrap(), bytes);
            assert!(workspaces(dir.path()).is_empty());
        }
    }
    #[test]
    fn mixed_header_only_sources_and_empty_schema_cases_are_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let a = source_bytes("id;left\n", Encoding::Utf16Be);
        let b = source_bytes("key;right\né;ré\n", Encoding::Latin1);
        let mut args = inputs(dir.path(), &a, &b);
        args[0].encoding = None;
        args[1].encoding = Some(Encoding::Latin1);
        for input in &mut args {
            input.delimiter = Some(b';');
        }
        let p = analyze_join(
            args,
            dir.path().to_path_buf(),
            JoinConfig {
                mode: JoinMode::Full,
                ..config()
            },
        )
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
        assert_eq!(p.analysis.input_rows, [0, 1]);
        assert_eq!(p.analysis.output_rows, 1);
        let output = dir.path().join("header-only.csv");
        p.start_write(output.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(output).unwrap(),
            "id;left;key;right\n;;é;ré\n".as_bytes()
        );
        for empty in [Vec::new(), vec![0xfe, 0xff]] {
            let mut args = inputs(dir.path(), &empty, b"id\nx\n");
            args[0].encoding = None;
            args[0].has_header = false;
            assert!(
                analyze_join(args, dir.path().to_path_buf(), config())
                    .unwrap()
                    .wait()
                    .is_err()
            );
        }
        assert!(workspaces(dir.path()).is_empty());
    }
}
