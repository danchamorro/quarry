//! Bounded, source-preserving joins. The initial desktop slice supports saved,
//! headered UTF-8 inputs, one unique key per side, and a left join.
use std::borrow::Cow;
use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};

use quarry_delimited::parse_record_with_field_limit;

use crate::encoding::RecordScanner;
use crate::export::{ExportTarget, source_matches_stamp};
use crate::spill::{self, FrameHead, FrameLimits, FrameReadError, SpillWorkspace};
use crate::storage::output_parent;
use crate::{Encoding, FilterExportOutcome, QuarryError, SourceStamp, check_storage};

const RECORD_LIMIT: usize = 64 * 1024 * 1024;
const COLUMN_LIMIT: usize = 65_536;
const READ_CHUNK: usize = 64 * 1024;
const MAX_RUN_LEVELS: usize = 64;

/// Explicit interpretation of one saved input. Column positions are zero based.
#[derive(Clone, Debug)]
pub struct JoinInput {
    pub path: PathBuf,
    pub delimiter: Option<u8>,
    pub has_header: bool,
    pub encoding: Option<Encoding>,
    pub key_column: usize,
}

/// Resource limits are internal policy, shared by analysis and creation.
#[derive(Clone, Copy, Debug)]
pub struct JoinConfig {
    pub memory_bytes: usize,
    pub run_memory_bytes: usize,
    pub max_record_bytes: usize,
    pub max_columns: usize,
    pub max_merge_fan_in: usize,
    pub preview_rows: usize,
    pub preview_bytes: usize,
}

impl Default for JoinConfig {
    fn default() -> Self {
        Self {
            memory_bytes: 512 * 1024 * 1024,
            run_memory_bytes: 16 * 1024 * 1024,
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
    pub delimiter: u8,
    pub columns: Vec<String>,
    pub data_rows: u64,
    pub empty_key_rows: u64,
    pub bytes: u64,
}

/// Counts describe the entire input; only preview values are sampled and clipped.
#[derive(Debug)]
pub struct JoinAnalysis {
    pub input_rows: [u64; 2],
    pub matching_rows: [u64; 2],
    pub matching_pairs: u64,
    pub unmatched_rows: [u64; 2],
    pub empty_key_rows: [u64; 2],
    pub output_rows: u64,
    pub output_bytes: u64,
    pub additional_temporary_bytes: u64,
    pub preview: Vec<Vec<String>>,
}

/// Immutable analysis owns its private sorted inputs until creation or release.
pub struct JoinPlan {
    inputs: [PreparedInput; 2],
    analysis: JoinAnalysis,
    workspace: Option<SpillWorkspace>,
    config: JoinConfig,
}

#[derive(Debug)]
pub struct JoinSummary {
    pub destination: PathBuf,
    pub data_rows: u64,
    pub bytes_written: u64,
    pub peak_temporary_bytes: u64,
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
    cancel: AtomicBool,
    done: AtomicBool,
}

impl Progress {
    fn new() -> Self {
        Self {
            phase: AtomicUsize::new(0),
            records: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            temporary: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            done: AtomicBool::new(false),
        }
    }

    fn check(&self) -> JResult<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(Failure::Cancelled)
        } else {
            Ok(())
        }
    }

    fn phase(&self, phase: JoinPhase) {
        self.phase.store(phase as usize, Ordering::Release);
        self.records.store(0, Ordering::Release);
    }

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
    fn spawn(
        work: impl FnOnce(&Progress) -> JResult<T> + Send + 'static,
    ) -> Result<Self, QuarryError> {
        let state = Arc::new(Progress::new());
        let worker = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("quarry-join".into())
            .spawn(move || {
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
    pub fn cancel(&self) {
        self.state.cancel.store(true, Ordering::Release);
    }

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

enum Failure {
    Cancelled,
    Error(QuarryError),
}
type JResult<T> = Result<T, Failure>;
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

fn error(reason: impl ToString) -> QuarryError {
    QuarryError::JoinFile {
        path: None,
        record: None,
        reason: reason.to_string(),
    }
}
fn input_error(path: &Path, record: Option<u64>, reason: impl ToString) -> QuarryError {
    QuarryError::JoinFile {
        path: Some(path.to_path_buf()),
        record,
        reason: reason.to_string(),
    }
}
fn add(left: u64, right: u64) -> JResult<u64> {
    left.checked_add(right)
        .ok_or_else(|| error("Join size or row count exceeds the supported range.").into())
}
fn mul(left: u64, right: u64) -> JResult<u64> {
    left.checked_mul(right)
        .ok_or_else(|| error("Join size or row count exceeds the supported range.").into())
}

impl JoinConfig {
    fn validate(self, temporary: &Path) -> Result<(), QuarryError> {
        if self.memory_bytes > 512 * 1024 * 1024
            || self.max_record_bytes == 0
            || self.max_record_bytes > RECORD_LIMIT
            || self.max_columns == 0
            || self.max_columns > COLUMN_LIMIT
            || self.run_memory_bytes == 0
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
        let path_bytes = temporary.as_os_str().len().saturating_add(128);
        let reserved = self
            .max_record_bytes
            .saturating_mul(7)
            .saturating_add(self.run_memory_bytes)
            .saturating_add(self.preview_bytes)
            .saturating_add(self.max_columns.saturating_mul(64))
            .saturating_add(
                MAX_RUN_LEVELS
                    .saturating_mul(self.max_merge_fan_in)
                    .saturating_mul(path_bytes),
            )
            .saturating_add(READ_CHUNK.saturating_mul(self.max_merge_fan_in + 1));
        if reserved > self.memory_bytes {
            return Err(error(
                "The join memory budget cannot fit its record, preview and merge limits.",
            ));
        }
        Ok(())
    }

    fn fan_in(self, max_key: usize) -> usize {
        // Headers and one record remain live while merging. Keep an additional
        // head allowance for advancing a stream, plus fixed I/O and path state.
        let reserved = self
            .max_record_bytes
            .saturating_mul(3)
            .saturating_add(self.run_memory_bytes)
            .saturating_add(self.preview_bytes)
            .saturating_add(self.max_columns.saturating_mul(64))
            .saturating_add(4 * 1024 * 1024);
        let head = max_key.saturating_add(8192 + 64).max(1);
        self.max_merge_fan_in
            .min(self.memory_bytes.saturating_sub(reserved) / head)
            .max(2)
    }

    fn frame_limits(self) -> FrameLimits {
        FrameLimits {
            max_key_bytes: self.max_record_bytes.saturating_add(9),
            max_record_bytes: self.max_record_bytes,
        }
    }
}

#[derive(Debug)]
struct PreparedInput {
    info: JoinInputInfo,
    stamp: SourceStamp,
    key_column: usize,
    sorted: PathBuf,
    frame_bytes: u64,
}

struct Entry {
    key: Vec<u8>,
    record: Vec<u8>,
    ordinal: u64,
}
struct Run {
    path: PathBuf,
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

fn read_head(
    reader: &mut impl Read,
    config: JoinConfig,
    state: &Progress,
) -> JResult<Option<FrameHead>> {
    spill::read_head(&mut CancelRead { reader, state }, config.frame_limits())
        .map_err(|error| frame_error(error, state))
}
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
            let count = spill::write_frame(&mut writer, &entry.key, &entry.record, entry.ordinal)?;
            bytes = add(bytes, count)?;
            self.state.add_disk(count)?;
        }
        writer.flush()?;
        drop(writer);
        self.carry(Run { path, bytes }, 0)
    }

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
            pending = self.merge(group)?;
            level += 1;
        }
    }

    fn merge(&self, runs: Vec<Run>) -> JResult<Run> {
        self.state.check()?;
        let mut readers: Vec<_> = runs
            .iter()
            .map(|run| File::open(&run.path).map(BufReader::new))
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
                spill::write_frame(&mut writer, &entry.head.key, &record, entry.head.ordinal)?;
            bytes = add(bytes, count)?;
            self.state.add_disk(count)?;
            let stream = entry.stream;
            drop(record);
            drop(entry);
            if let Some(head) = read_head(&mut readers[stream], self.config, self.state)? {
                heap.push(HeapEntry { head, stream });
            }
        }
        writer.flush()?;
        drop(writer);
        drop(readers);
        for run in runs {
            fs::remove_file(run.path)?;
            self.state.temporary.fetch_sub(run.bytes, Ordering::AcqRel);
        }
        Ok(Run { path, bytes })
    }

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
    let file = File::open(&input.info.path).map_err(|e| input_error(&input.info.path, None, e))?;
    if !source_matches_stamp(&file, &input.info.path, &input.stamp)? {
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
fn scan(
    path: &Path,
    stamp: &SourceStamp,
    delimiter: u8,
    config: JoinConfig,
    state: &Progress,
    mut visit: impl FnMut(&[u8], u64) -> JResult<()>,
) -> JResult<()> {
    let mut file = File::open(path).map_err(|e| input_error(path, None, e))?;
    if !source_matches_stamp(&file, path, stamp)? {
        return Err(QuarryError::SourceChanged.into());
    }
    let mut scanner = RecordScanner::new(delimiter, Encoding::Utf8)?;
    let mut chunk = vec![0; READ_CHUNK.min(config.max_record_bytes)];
    let mut record = Vec::new();
    let mut offset = 0_u64;
    let mut row = 0_u64;
    loop {
        state.check()?;
        let read = file.read(&mut chunk)?;
        if read == 0 {
            let mut last = false;
            scanner
                .finish(offset, |_| last = true)
                .map_err(|e| input_error(path, Some(row + 1), e))?;
            if last {
                visit(&record, row)?;
            }
            if !source_matches_stamp(&file, path, stamp)? {
                return Err(QuarryError::SourceChanged.into());
            }
            return Ok(());
        }
        if add(offset, read as u64)? > stamp.file_size() {
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
                        path,
                        Some(row + 1),
                        "Record exceeds the configured size limit.",
                    )
                    .into(),
                );
                return;
            }
            record.extend_from_slice(&chunk[start..end]);
            if let Err(error) = visit(&record, row) {
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
        scanned.map_err(|e| input_error(path, Some(row + 1), e))?;
        if record.len().saturating_add(read - start) > config.max_record_bytes {
            return Err(input_error(
                path,
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

fn key(fields: &[Cow<'_, [u8]>], column: usize, ordinal: u64) -> Vec<u8> {
    match fields.get(column).filter(|field| !field.is_empty()) {
        Some(value) => {
            let mut result = Vec::with_capacity(value.len() + 1);
            result.push(1);
            result.extend_from_slice(value);
            result
        }
        None => {
            let mut result = vec![0];
            result.extend_from_slice(&ordinal.to_be_bytes());
            result
        }
    }
}

struct Inspected {
    info: JoinInputInfo,
    stamp: SourceStamp,
    key_column: usize,
    frame_bytes: u64,
    max_key_bytes: usize,
}

fn inspect(input: JoinInput, config: JoinConfig, state: &Progress) -> JResult<Inspected> {
    state.phase(JoinPhase::Inspecting);
    if !input.has_header {
        return Err(input_error(
            &input.path,
            None,
            "This first join version requires a header row in both files.",
        )
        .into());
    }
    if input
        .encoding
        .is_some_and(|encoding| encoding != Encoding::Utf8)
    {
        return Err(input_error(
            &input.path,
            None,
            "This first join version requires UTF-8 inputs.",
        )
        .into());
    }
    let path = if input.path.is_absolute() {
        input.path
    } else {
        std::env::current_dir()?.join(input.path)
    };
    let mut file = File::open(&path).map_err(|e| input_error(&path, None, e))?;
    if !file.metadata()?.is_file() {
        return Err(input_error(&path, None, "Select a regular file.").into());
    }
    let stamp = SourceStamp::from_file(&file)?;
    let encoding =
        Encoding::detect(&mut file, input.encoding).map_err(|e| input_error(&path, None, e))?;
    if encoding != Encoding::Utf8 {
        return Err(input_error(
            &path,
            None,
            "This first join version requires UTF-8 inputs.",
        )
        .into());
    }
    let delimiter = match input.delimiter {
        Some(value) => {
            RecordScanner::new(value, Encoding::Utf8)?;
            value
        }
        None => crate::detect_file_delimiter(
            &mut file,
            stamp.file_size(),
            crate::DEFAULT_SAMPLE_BYTES,
            crate::DEFAULT_BOOTSTRAP_LIMIT,
            Encoding::Utf8,
        )?,
    };
    drop(file);
    let mut columns = None;
    let mut data_rows = 0;
    let mut empty_key_rows = 0;
    let mut frame_bytes = 0;
    let mut max_key_bytes = 0;
    scan(&path, &stamp, delimiter, config, state, |record, row| {
        let parsed = fields(record, delimiter, row == 0, &path, row, config)?;
        if row == 0 {
            let names: Vec<_> = parsed
                .iter()
                .map(|field| String::from_utf8(field.to_vec()).expect("validated UTF-8"))
                .collect();
            if input.key_column >= names.len() {
                return Err(input_error(
                    &path,
                    Some(1),
                    "The selected key column is outside the header.",
                )
                .into());
            }
            columns = Some(names);
        } else {
            if parsed.len() > columns.as_ref().map_or(0, Vec::len) {
                return Err(input_error(&path, Some(row+1), "A row contains more columns than its header. This join version requires named columns.").into());
            }
            let key = key(&parsed, input.key_column, data_rows);
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
        }
        Ok(())
    })?;
    let columns = columns
        .ok_or_else(|| input_error(&path, None, "The file is empty and has no header schema."))?;
    Ok(Inspected {
        info: JoinInputInfo {
            path,
            delimiter,
            columns,
            data_rows,
            empty_key_rows,
            bytes: stamp.file_size(),
        },
        stamp,
        key_column: input.key_column,
        frame_bytes,
        max_key_bytes,
    })
}

fn same_input(left: &Inspected, right: &Inspected) -> JResult<bool> {
    if left.info.path.canonicalize()? == right.info.path.canonicalize()? {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = fs::metadata(&left.info.path)?;
        let b = fs::metadata(&right.info.path)?;
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    #[cfg(not(unix))]
    {
        // Conservatively refuse indistinguishable saved identities on platforms
        // without the Unix inode guard; never risk a same-source join.
        Ok(left.stamp.matches(&right.stamp))
    }
}

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
        &input.info.path,
        &input.stamp,
        input.info.delimiter,
        config,
        state,
        |record, row| {
            if row == 0 {
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
            let comparison = key(&parsed, input.key_column, rows);
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
        stamp: input.stamp,
        key_column: input.key_column,
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

fn output_header(
    inputs: &[PreparedInput; 2],
    config: JoinConfig,
    state: &Progress,
) -> JResult<Vec<u8>> {
    use std::collections::{HashMap, HashSet};
    let mut counts = HashMap::new();
    for input in inputs {
        for name in &input.info.columns {
            *counts.entry(name.as_str()).or_insert(0_usize) += 1;
        }
    }
    let mut used = HashSet::new();
    let mut suffixes: HashMap<String, usize> = HashMap::new();
    let mut names = Vec::new();
    let mut name_bytes = 0_usize;
    for (side, input) in inputs.iter().enumerate() {
        let role = if side == 0 { "Main" } else { "Additional" };
        for (column, name) in input.info.columns.iter().enumerate() {
            state.check()?;
            let base = if name.is_empty() {
                format!("{role} column {}", column + 1)
            } else if counts[name.as_str()] > 1 {
                format!("{name} ({role})")
            } else {
                name.clone()
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
            // Bound intermediate names before serialization as well as the
            // final escaped header. Repeated names never restart suffix scans.
            name_bytes = name_bytes.saturating_add(candidate.len()).saturating_add(1);
            if name_bytes > config.max_record_bytes {
                return Err(error("The joined header exceeds the configured size limit.").into());
            }
            used.insert(candidate.clone());
            names.push(candidate);
        }
    }
    encode(
        names.iter().map(|name| name.as_bytes()),
        inputs[0].info.delimiter,
        config.max_record_bytes,
    )
}

fn output_record(
    inputs: &[PreparedInput; 2],
    left: &[u8],
    right: Option<&[u8]>,
    config: JoinConfig,
) -> JResult<Vec<u8>> {
    let a = fields(
        left,
        inputs[0].info.delimiter,
        false,
        &inputs[0].info.path,
        0,
        config,
    )?;
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
    let values = (0..inputs[0].info.columns.len())
        .map(|i| a.get(i).map_or(b"".as_slice(), |v| v.as_ref()))
        .chain(
            (0..inputs[1].info.columns.len())
                .map(|i| b.get(i).map_or(b"".as_slice(), |v| v.as_ref())),
        );
    encode(values, inputs[0].info.delimiter, config.max_record_bytes)
}

struct Stream<'a> {
    input: &'a PreparedInput,
    reader: BufReader<File>,
    head: Option<FrameHead>,
    config: JoinConfig,
    state: &'a Progress,
}
impl<'a> Stream<'a> {
    fn new(input: &'a PreparedInput, config: JoinConfig, state: &'a Progress) -> JResult<Self> {
        let mut reader = BufReader::new(File::open(&input.sorted)?);
        let head = read_head(&mut reader, config, state)?;
        Ok(Self {
            input,
            reader,
            head,
            config,
            state,
        })
    }
    fn record(&mut self) -> JResult<(Vec<u8>, u64)> {
        let head = self.head.take().expect("stream head");
        let record = read_record(&mut self.reader, head.record_len, self.config, self.state)?;
        let parsed = fields(
            &record,
            self.input.info.delimiter,
            false,
            &self.input.info.path,
            head.ordinal + 1,
            self.config,
        )?;
        let value = parsed
            .get(self.input.key_column)
            .filter(|value| !value.is_empty());
        let matches = match value {
            Some(value) => {
                head.key.first() == Some(&1) && head.key.get(1..) == Some(value.as_ref())
            }
            None => {
                head.key.first() == Some(&0)
                    && head.key.get(1..) == Some(head.ordinal.to_be_bytes().as_slice())
            }
        };
        if !matches {
            return Err(error("Private join key differs from its record.").into());
        }
        drop(parsed);
        Ok((record, head.ordinal))
    }
    fn advance(&mut self) -> JResult<()> {
        self.head = read_head(&mut self.reader, self.config, self.state)?;
        Ok(())
    }
}

/// Visit equality matches and both kinds of unmatched rows. Callback payloads
/// are released before fetching more heads, keeping the live set bounded.
fn walk(
    inputs: &[PreparedInput; 2],
    config: JoinConfig,
    state: &Progress,
    mut visit: impl FnMut(Option<(&[u8], u64)>, Option<(&[u8], u64)>) -> JResult<()>,
) -> JResult<()> {
    let mut a = Stream::new(&inputs[0], config, state)?;
    let mut b = Stream::new(&inputs[1], config, state)?;
    loop {
        state.check()?;
        let order = match (&a.head, &b.head) {
            (None, None) => break,
            (Some(_), None) => CmpOrdering::Less,
            (None, Some(_)) => CmpOrdering::Greater,
            (Some(left), Some(_)) if left.key.first() == Some(&0) => CmpOrdering::Less,
            (Some(_), Some(right)) if right.key.first() == Some(&0) => CmpOrdering::Greater,
            (Some(left), Some(right)) => left.key.cmp(&right.key),
        };
        match order {
            CmpOrdering::Less => {
                let (record, ordinal) = a.record()?;
                visit(Some((&record, ordinal)), None)?;
                drop(record);
                a.advance()?;
            }
            CmpOrdering::Greater => {
                let (record, ordinal) = b.record()?;
                visit(None, Some((&record, ordinal)))?;
                drop(record);
                b.advance()?;
            }
            CmpOrdering::Equal => {
                let (left, left_ordinal) = a.record()?;
                let (right, right_ordinal) = b.record()?;
                visit(Some((&left, left_ordinal)), Some((&right, right_ordinal)))?;
                drop(left);
                drop(right);
                a.advance()?;
                b.advance()?;
            }
        }
    }
    Ok(())
}

fn validate_keys(input: &PreparedInput, config: JoinConfig, state: &Progress) -> JResult<()> {
    let mut reader = BufReader::new(File::open(&input.sorted)?);
    let mut previous: Option<FrameHead> = None;
    let mut count = 0;
    while let Some(head) = read_head(&mut reader, config, state)? {
        if head.key.is_empty() {
            return Err(error("Empty private join key.").into());
        }
        if let Some(previous) = &previous {
            if previous.key > head.key
                || (previous.key == head.key && previous.ordinal >= head.ordinal)
            {
                return Err(error("Private join records are out of order.").into());
            }
            if head.key[0] == 1 && previous.key == head.key {
                return Err(input_error(
                    &input.info.path,
                    Some(head.ordinal + 2),
                    "Repeated key found. This first join version requires unique nonempty keys.",
                )
                .into());
            }
        }
        let mut body = CancelRead {
            reader: &mut reader,
            state,
        }
        .take(head.record_len as u64);
        if io::copy(&mut body, &mut io::sink()).map_err(|e| {
            if state.cancel.load(Ordering::Acquire) {
                Failure::Cancelled
            } else {
                e.into()
            }
        })? != head.record_len as u64
        {
            return Err(error("Truncated private join record.").into());
        }
        previous = Some(head);
        count = add(count, 1)?;
    }
    if count != input.info.data_rows {
        return Err(error("Prepared join row count changed.").into());
    }
    Ok(())
}

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

fn analyze_prepared(
    inputs: &[PreparedInput; 2],
    config: JoinConfig,
    state: &Progress,
) -> JResult<JoinAnalysis> {
    state.phase(JoinPhase::Analyzing);
    for input in inputs {
        validate_keys(input, config, state)?;
    }
    let header_bytes = output_header(inputs, config, state)?.len() as u64;
    let mut matching = 0;
    let mut unmatched = [0, 0];
    let mut output_rows = 0;
    let mut output_bytes = header_bytes;
    let mut preview = std::collections::BTreeMap::new();
    walk(inputs, config, state, |left, right| {
        match (left.is_some(), right.is_some()) {
            (true, true) => matching = add(matching, 1)?,
            (true, false) => unmatched[0] = add(unmatched[0], 1)?,
            (false, true) => unmatched[1] = add(unmatched[1], 1)?,
            _ => unreachable!(),
        }
        if let Some((left, ordinal)) = left {
            let row = output_record(inputs, left, right.map(|(record, _)| record), config)?;
            output_rows = add(output_rows, 1)?;
            output_bytes = add(output_bytes, row.len() as u64)?;
            if config.preview_rows > 0
                && (preview.len() < config.preview_rows
                    || preview
                        .last_key_value()
                        .is_some_and(|(&last, _)| ordinal < last))
            {
                preview.insert(
                    ordinal,
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
        }
        state.records.fetch_add(1, Ordering::Release);
        Ok(())
    })?;
    if output_rows != inputs[0].info.data_rows {
        return Err(error("Analyzed join row count differs from Main.").into());
    }
    let frames = add(output_bytes - header_bytes, mul(output_rows, 32)?)?;
    let temporary = add(mul(frames, 2)?, output_bytes)?;
    for input in inputs {
        check_source(input)?;
    }
    Ok(JoinAnalysis {
        input_rows: [inputs[0].info.data_rows, inputs[1].info.data_rows],
        matching_rows: [matching, matching],
        matching_pairs: matching,
        unmatched_rows: unmatched,
        empty_key_rows: [inputs[0].info.empty_key_rows, inputs[1].info.empty_key_rows],
        output_rows,
        output_bytes,
        additional_temporary_bytes: temporary,
        preview: preview.into_values().collect(),
    })
}

fn cleanup_failure(workspace: SpillWorkspace, failure: Failure) -> Failure {
    match workspace.cleanup() {
        Ok(()) => failure,
        Err(cleanup) => {
            let cause = match failure {
                Failure::Cancelled => "Join cancelled".to_string(),
                Failure::Error(error) => error.to_string(),
            };
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
    config.validate(&temporary)?;
    JoinJob::spawn(move |state| {
        let [left, right] = inputs;
        let left = inspect(left, config, state)?;
        let right = inspect(right, config, state)?;
        if same_input(&left, &right)? {
            return Err(
                error("Choose two different input files. Self joins are not supported.").into(),
            );
        }
        if left
            .info
            .columns
            .len()
            .saturating_add(right.info.columns.len())
            > config.max_columns
        {
            return Err(error("The result has too many columns.").into());
        }
        let initial = mul(add(left.frame_bytes, right.frame_bytes)?, 2)?;
        check_storage(&temporary, initial)?;
        state.check()?;
        let workspace = SpillWorkspace::create_in(&temporary, "join")?;
        let prepared = (|| {
            let left = prepare(left, &workspace, config, state)?;
            let right = prepare(right, &workspace, config, state)?;
            let inputs = [left, right];
            let analysis = analyze_prepared(&inputs, config, state)?;
            Ok((inputs, analysis))
        })();
        match prepared {
            Ok((inputs, analysis)) => Ok(JoinPlan {
                inputs,
                analysis,
                workspace: Some(workspace),
                config,
            }),
            Err(failure) => Err(cleanup_failure(workspace, failure)),
        }
    })
}

fn check_creation_space(
    temporary: &Path,
    destination: &Path,
    spill_bytes: u64,
    output_bytes: u64,
) -> JResult<()> {
    #[cfg(unix)]
    let same_volume = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(temporary)?.dev() == fs::metadata(output_parent(destination))?.dev()
    };
    #[cfg(not(unix))]
    let same_volume = true;
    if same_volume {
        check_storage(temporary, add(spill_bytes, output_bytes)?)?;
    } else {
        check_storage(temporary, spill_bytes)?;
        check_storage(output_parent(destination), output_bytes)?;
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

    /// Consume the immutable analyzed plan and atomically create a new file.
    pub fn start_write(
        mut self,
        destination: PathBuf,
    ) -> Result<JoinJob<JoinSummary>, QuarryError> {
        let workspace = self.workspace.take().expect("join workspace");
        JoinJob::spawn(move |state| {
            let result = self.write_inner(&workspace, &destination, state);
            match result {
                Ok(mut summary) => {
                    if let Err(error) = workspace.cleanup() {
                        summary.cleanup_warning = Some(error.to_string());
                    }
                    Ok(summary)
                }
                Err(failure) => Err(cleanup_failure(workspace, failure)),
            }
        })
    }

    fn write_inner(
        &self,
        workspace: &SpillWorkspace,
        destination: &Path,
        state: &Progress,
    ) -> JResult<JoinSummary> {
        state.phase(JoinPhase::Creating);
        state.check()?;
        let retained = add(self.inputs[0].frame_bytes, self.inputs[1].frame_bytes)?;
        state.add_disk(retained)?;
        for input in &self.inputs {
            check_source(input)?;
        }
        let header = output_header(&self.inputs, self.config, state)?;
        let frame_bytes = add(
            self.analysis.output_bytes - header.len() as u64,
            mul(self.analysis.output_rows, 32)?,
        )?;
        check_creation_space(
            &workspace.path,
            destination,
            mul(frame_bytes, 2)?,
            self.analysis.output_bytes,
        )?;
        let sources = self
            .inputs
            .iter()
            .map(|input| (input.info.path.clone(), input.stamp.clone()))
            .collect();
        let mut output = ExportTarget::new_combined(sources, destination.to_path_buf())?;
        output.configure(Encoding::Utf8, Encoding::Utf8, self.config.max_record_bytes);
        let prepared = (|| {
            output.write_all(&header)?;
            state.add_disk(header.len() as u64)?;
            drop(header);
            let mut runs = Runs::new(workspace, self.config, state, 8);
            let mut count = 0;
            let mut bytes = 0;
            walk(&self.inputs, self.config, state, |left, right| {
                if let Some((left, ordinal)) = left {
                    let row = output_record(
                        &self.inputs,
                        left,
                        right.map(|(record, _)| record),
                        self.config,
                    )?;
                    count = add(count, 1)?;
                    bytes = add(bytes, row.len() as u64)?;
                    runs.push(Entry {
                        key: ordinal.to_be_bytes().to_vec(),
                        record: row,
                        ordinal,
                    })?;
                    state.records.store(count, Ordering::Release);
                }
                Ok(())
            })?;
            let ordered = runs.finish()?;
            if count != self.analysis.output_rows
                || add(bytes, output.bytes_written())? != self.analysis.output_bytes
                || ordered.bytes != frame_bytes
            {
                return Err(
                    error("Join output differs from the analyzed plan. Analyze again.").into(),
                );
            }
            let mut reader = BufReader::new(File::open(&ordered.path)?);
            let mut ordinal = 0;
            while let Some(head) = read_head(&mut reader, self.config, state)? {
                if head.ordinal != ordinal || head.key != ordinal.to_be_bytes() {
                    return Err(error("Joined rows are not in source order.").into());
                }
                let record = read_record(&mut reader, head.record_len, self.config, state)?;
                let parsed = fields(
                    &record,
                    self.inputs[0].info.delimiter,
                    false,
                    destination,
                    ordinal,
                    self.config,
                )?;
                if parsed.len()
                    != self.inputs[0].info.columns.len() + self.inputs[1].info.columns.len()
                {
                    return Err(error("Joined row has the wrong number of columns.").into());
                }
                drop(parsed);
                output.write_all(&record)?;
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
                    "Join failed or was cancelled. Could not remove output staging: {cleanup}"
                ))
                .into(),
            });
        }
        state.phase(JoinPhase::Publishing);
        match output.publish(
            self.analysis.output_rows,
            self.analysis.output_bytes,
            &state.cancel,
        )? {
            FilterExportOutcome::Cancelled => Err(Failure::Cancelled),
            FilterExportOutcome::Complete(_) => Ok(JoinSummary {
                destination: destination.to_path_buf(),
                data_rows: self.analysis.output_rows,
                bytes_written: self.analysis.output_bytes,
                peak_temporary_bytes: state.peak.load(Ordering::Acquire),
                cleanup_warning: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn config() -> JoinConfig {
        JoinConfig {
            memory_bytes: 512 * 1024,
            run_memory_bytes: 128,
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
            key_column: 0,
        })
    }
    fn plan(dir: &Path, a: &[u8], b: &[u8]) -> JoinPlan {
        analyze_join(inputs(dir, a, b), dir.to_path_buf(), config())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap()
    }
    fn failure(dir: &Path, a: &[u8], b: &[u8]) -> String {
        match analyze_join(inputs(dir, a, b), dir.to_path_buf(), config())
            .unwrap()
            .wait()
        {
            Err(error) => error.to_string(),
            _ => panic!("expected failure"),
        }
    }
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
    fn invalid_unselected_text_and_late_extra_fields_block_analysis() {
        let dir = tempfile::tempdir().unwrap();
        assert!(failure(dir.path(), b"id,label\nx,\xff\n", b"id\nx\n").contains("Invalid UTF-8"));
        assert!(failure(dir.path(), b"id\nx\nx,late\n", b"id\nx\n").contains("more columns"));
        assert!(failure(dir.path(), b"id\n\"unfinished", b"id\nx\n").contains("quote"));
        assert!(failure(dir.path(), b"", b"id\nx\n").contains("empty"));
        assert!(workspaces(dir.path()).is_empty());
    }

    #[test]
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
    fn exact_case_and_whitespace_are_significant_and_key_can_be_later_column() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = inputs(
            dir.path(),
            b"value,id\nupper,A\nlower,a\nspace, a\nmissing\n",
            b"name,key\nmatch,a\n",
        );
        args.iter_mut().for_each(|input| input.key_column = 1);
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
    fn unsupported_formats_self_joins_and_inadequate_budgets_are_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = inputs(dir.path(), b"id\nx\n", b"id\nx\n");
        args[1].has_header = false;
        assert!(
            analyze_join(args, dir.path().to_path_buf(), config())
                .unwrap()
                .wait()
                .is_err()
        );
        let mut args = inputs(dir.path(), b"id\nx\n", b"id\nx\n");
        args[1].encoding = Some(Encoding::Latin1);
        assert!(
            analyze_join(args, dir.path().to_path_buf(), config())
                .unwrap()
                .wait()
                .is_err()
        );
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
    fn cancelling_a_running_writer_releases_spills_and_keeps_sources() {
        let dir = tempfile::tempdir().unwrap();
        let a = format!(
            "id,value\n{}",
            (0..20000)
                .map(|i| format!("{i:08},value-{i}\n"))
                .collect::<String>()
        );
        let mut cfg = config();
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
    fn many_identical_headers_have_unique_bounded_names() {
        let dir = tempfile::tempdir().unwrap();
        let header = format!("{}\n", vec!["same"; 2048].join(","));
        let args = inputs(dir.path(), header.as_bytes(), b"extra\n");
        let cfg = JoinConfig {
            memory_bytes: 2 * 1024 * 1024,
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
}
