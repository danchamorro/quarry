mod case;
mod cleanup;
mod combine;
mod date_time_sort;
mod destination;
mod encoding;
mod export;
mod filter;
mod index;
mod inspection;
mod join;

mod search;
mod sort;
mod source_stamp;
mod spill;
mod storage;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use source_stamp::SourceStamp;

pub use case::CaseSensitivity;
pub use cleanup::{CleanupOperation, ColumnCleanup};
pub use combine::{
    CombineInput, CombineJob, CombineOptions, CombinePlan, CombineProgress, CombineSummary,
    check_combine_files, check_combine_files_with_encoding,
};
pub use date_time_sort::DateTimeFormat;
pub use encoding::Encoding;
use encoding::RecordScanner;
pub use export::{
    CleanupJob, CleanupOutcome, CleanupSummary, ColumnTransformation, FilterExportJob,
    FilterExportOutcome, FilterExportProgress, FilterExportSummary, LiteralReplacement,
    MAX_TRANSFORMATION_COLUMNS, ReplaceAllJob, ReplaceAllOutcome, ReplaceAllSummary, RowInsertion,
    SaveAsJob, SaveAsOutcome, SaveAsProgress, SaveAsSummary, SplitAnalysisJob,
    SplitAnalysisOutcome, SplitAnalysisProgress, SplitAnalysisSummary,
};
pub use filter::{
    FilterEdits, FilterIndex, FilterJob, FilterMatch, FilterOperator, FilterPredicate,
    FilterProgress, FilterQuery, FilterReadJob, FilterReadOutcome, FilterReadProgress,
};
pub use index::{
    Checkpoint, CompletedIndex, IndexConfig, IndexJob, IndexProgress, StructuralIndex,
};
pub use inspection::{
    Diagnostic, DiagnosticKind, InspectionEdits, InspectionJob, InspectionOptions,
    InspectionProgress, InspectionReport, InspectionStatus, RecoveryJob, RecoverySummary,
    SourcePreview,
};
pub use join::{
    JoinAnalysis, JoinCase, JoinConfig, JoinDuplicateExample, JoinFormat, JoinHeader, JoinInput,
    JoinInputInfo, JoinJob, JoinMode, JoinPhase, JoinPlan, JoinProgress, JoinSourceVersion,
    JoinSummary, analyze_join,
};
pub use join::{
    JoinCellEdits, JoinHeaderEdits, JoinInputOwner, JoinWorkingSource, analyze_join_with_working,
    read_join_working_header,
};
pub use join::{
    JoinOutputColumn, JoinOutputOptions, JoinSource, analyze_join_with_output, read_join_header,
    read_join_header_with_format,
};
use quarry_delimited::{ParseError, parse_record};
pub use search::{SearchJob, SearchMatch, SearchOutcome, SearchPosition, SearchProgress};
pub use sort::{
    DuplicateJob, DuplicateOutcome, DuplicateProgress, DuplicateSpec, DuplicateSummary,
    MAX_SORT_KEYS, SortDirection, SortJob, SortMode, SortOutcome, SortPlan, SortProgress, SortSpec,
    SortSummary, estimate_multi_sort_temporary_bytes, estimate_sort_temporary_bytes,
};
pub use storage::{
    StorageSpace, check_storage, estimate_duplicate_temporary_bytes, estimate_edited_output_bytes,
    estimate_edited_output_bytes_from_values, inspect_storage,
};

const DEFAULT_SAMPLE_BYTES: usize = 1024 * 1024;
const DEFAULT_BOOTSTRAP_LIMIT: usize = 64 * 1024 * 1024;
const DEFAULT_READ_CHUNK: usize = 1024 * 1024;
const DEFAULT_MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

pub(crate) fn parse_source_record(
    record: &[u8],
    delimiter: u8,
    physical_row: u64,
) -> Result<Vec<Cow<'_, [u8]>>, ParseError> {
    let record = if physical_row == 0 {
        record.strip_prefix(UTF8_BOM).unwrap_or(record)
    } else {
        record
    };
    parse_record(record, delimiter)
}

/// Parses native record bytes and wraps record-local decoding failures with their logical record.
pub(crate) fn parse_encoded_record(
    record: &[u8],
    delimiter: u8,
    physical_row: u64,
    encoding: Encoding,
) -> Result<Vec<Cow<'_, [u8]>>, QuarryError> {
    if encoding == Encoding::Utf8 {
        return parse_source_record(record, delimiter, physical_row).map_err(Into::into);
    }
    let body = if physical_row == 0 {
        encoding.strip_bom(record).1
    } else {
        record
    };
    let decoded = encoding
        .decode(body, DEFAULT_MAX_RECORD_BYTES)
        .map_err(|error| record_encoding_error(physical_row.saturating_add(1), error))?;
    match decoded {
        Cow::Borrowed(bytes) => parse_record(bytes, delimiter).map_err(Into::into),
        Cow::Owned(bytes) => Ok(parse_record(&bytes, delimiter)?
            .into_iter()
            .map(|field| Cow::Owned(field.into_owned()))
            .collect()),
    }
}

/// Retains a logical record while separating decoder-local byte offsets from source positions.
pub(crate) fn record_encoding_error(record: u64, error: QuarryError) -> QuarryError {
    match error {
        error @ (QuarryError::Encoding(_) | QuarryError::EncodingAt { .. }) => {
            QuarryError::RecordEncoding {
                record,
                cause: Box::new(error),
            }
        }
        error => error,
    }
}

#[derive(Debug)]
pub enum QuarryError {
    JoinFile {
        path: Option<PathBuf>,
        record: Option<u64>,
        reason: String,
    },
    CombineFile {
        path: PathBuf,
        record: Option<u64>,
        reason: String,
    },
    Io(io::Error),
    Storage {
        directory: PathBuf,
        error: io::Error,
    },
    InsufficientStorage {
        directory: PathBuf,
        required_bytes: u64,
        available_bytes: u64,
    },
    Parse(ParseError),
    Encoding(String),
    RecordEncoding {
        record: u64,
        cause: Box<QuarryError>,
    },
    EncodingAt {
        encoding: Encoding,
        offset: u64,
        reason: String,
    },
    BootstrapLimitExceeded {
        limit: usize,
        rows_found: usize,
    },
    RecordTooLarge {
        limit: usize,
    },
    RowNotIndexed {
        requested: u64,
        indexed_rows: u64,
    },
    MatchNotIndexed {
        requested: u64,
        indexed_matches: u64,
    },
    InvalidNumericSortValue {
        data_row: u64,
        column: usize,
    },
    InvalidCleanupText {
        data_row: u64,
        column: usize,
    },
    InvalidUtf8SortValue {
        data_row: u64,
        column: usize,
    },
    InvalidDateTimeSortValue {
        data_row: u64,
        column: usize,
        format: DateTimeFormat,
        reason: &'static str,
    },
    InvalidFilter {
        rule: usize,
        reason: &'static str,
    },
    InvalidOption(&'static str),
    ExportDestinationIsSource,
    ExportDestinationExists,
    SourceChanged,
    WorkerPanicked,
}

impl fmt::Display for QuarryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RecordEncoding { record, cause } => match cause.as_ref() {
                Self::EncodingAt {
                    encoding, reason, ..
                } => write!(
                    f,
                    "Record {record}: invalid {encoding} text: {reason}. Choose the correct encoding; no text was replaced."
                ),
                cause => write!(f, "Record {record}: {cause}"),
            },

            Self::JoinFile {
                path,
                record,
                reason,
            } => {
                if let Some(path) = path {
                    write!(f, "{}", path.display())?;
                } else {
                    f.write_str("Cannot join files")?;
                }
                if let Some(record) = record {
                    write!(f, ", record {record}")?;
                }
                write!(f, ": {reason}")
            }
            Self::CombineFile {
                path,
                record,
                reason,
            } => {
                write!(f, "{}", path.display())?;
                if let Some(record) = record {
                    write!(f, ", record {record}")?;
                }
                write!(f, ": {reason}")
            }
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Storage { directory, error } => write!(
                f,
                "Cannot use storage at {}: {error}. Check that the drive is connected, writable, and has free space, or choose another location. The current document is preserved.",
                directory.display()
            ),
            Self::InsufficientStorage {
                directory,
                required_bytes,
                available_bytes,
            } => write!(
                f,
                "Not enough space at {}: {required_bytes} additional bytes required, {available_bytes} bytes available. Free space or choose another location. Existing working and Undo files are preserved.",
                directory.display()
            ),
            Self::Parse(error) => error.fmt(f),
            Self::Encoding(message) => f.write_str(message),
            Self::EncodingAt {
                encoding,
                offset,
                reason,
            } => write!(
                f,
                "Invalid {encoding} at byte {offset}: {reason}. Choose the correct encoding; no text was replaced."
            ),
            Self::BootstrapLimitExceeded { limit, rows_found } => write!(
                f,
                "found only {rows_found} rows within the {limit}-byte bootstrap limit"
            ),
            Self::RecordTooLarge { limit } => {
                write!(f, "record exceeds the configured {limit}-byte limit")
            }
            Self::RowNotIndexed {
                requested,
                indexed_rows,
            } => write!(
                f,
                "row {requested} is not indexed yet ({indexed_rows} rows available)"
            ),
            Self::MatchNotIndexed {
                requested,
                indexed_matches,
            } => write!(
                f,
                "match {requested} is not indexed yet ({indexed_matches} matches available)"
            ),
            Self::InvalidNumericSortValue { data_row, column } => write!(
                f,
                "Cannot sort as Number at data row {data_row}, column {column}: expected a number (for example -12.5 or 1e3)."
            ),
            Self::InvalidCleanupText { data_row, column } => write!(
                f,
                "Cannot clean text at data row {data_row}, column {column}: expected valid UTF-8 text."
            ),
            Self::InvalidUtf8SortValue { data_row, column } => write!(
                f,
                "Cannot count text at data row {data_row}, column {column}: expected valid UTF-8 text."
            ),
            Self::InvalidDateTimeSortValue {
                data_row,
                column,
                format,
                reason,
            } => write!(
                f,
                "Cannot sort as Date / time at data row {data_row}, column {column} using {}: {reason}. Choose the matching format or correct the value. The current document is preserved.",
                format.label()
            ),
            Self::InvalidFilter { rule, reason } => {
                write!(f, "invalid filter rule {rule}: {reason}")
            }
            Self::InvalidOption(option) => write!(f, "invalid option: {option}"),
            Self::ExportDestinationIsSource => {
                write!(f, "export destination must differ from the source file")
            }
            Self::ExportDestinationExists => write!(f, "export destination already exists"),
            Self::SourceChanged => write!(f, "source file changed since it was opened"),
            Self::WorkerPanicked => write!(f, "background worker panicked"),
        }
    }
}

impl Error for QuarryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Storage { error, .. } => Some(error),
            Self::Parse(error) => Some(error),
            Self::RecordEncoding { cause, .. } => Some(cause.as_ref()),
            _ => None,
        }
    }
}

impl From<io::Error> for QuarryError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<ParseError> for QuarryError {
    fn from(value: ParseError) -> Self {
        Self::Parse(value)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum HeaderMode {
    #[default]
    Auto,
    FirstRow,
    NoHeader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenOptions {
    pub rows: usize,
    pub delimiter: Option<u8>,
    pub header_mode: HeaderMode,
    pub sample_bytes: usize,
    pub bootstrap_limit: usize,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            rows: 100,
            delimiter: None,
            header_mode: HeaderMode::Auto,
            sample_bytes: DEFAULT_SAMPLE_BYTES,
            bootstrap_limit: DEFAULT_BOOTSTRAP_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Dialect {
    pub delimiter: u8,
    pub has_header: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct OpenMetrics {
    pub file_open: Duration,
    pub first_rows: Duration,
    pub bootstrap_bytes_read: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub offset: u64,
    pub fields: Vec<Vec<u8>>,
}

#[derive(Debug)]
pub struct Session {
    path: PathBuf,
    source_stamp: SourceStamp,
    encoding: Encoding,
    completed_record_count: Arc<Mutex<Option<CompletedRecordCount>>>,
    pub file_size: u64,
    pub dialect: Dialect,
    pub first_rows: Vec<Row>,
    pub metrics: OpenMetrics,
}

#[derive(Debug, Clone, Copy)]
struct CompletedRecordCount {
    delimiter: u8,
    encoding: Encoding,
    file_size: u64,
    records: u64,
}

impl Session {
    pub fn open(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self, QuarryError> {
        Self::open_with_encoding(path, options, None)
    }

    /// Open using an explicit encoding, or detect only Unicode BOMs when None.
    pub fn open_with_encoding(
        path: impl AsRef<Path>,
        options: OpenOptions,
        requested_encoding: Option<Encoding>,
    ) -> Result<Self, QuarryError> {
        if options.rows == 0 || options.sample_bytes == 0 || options.bootstrap_limit == 0 {
            return Err(QuarryError::InvalidOption(
                "rows, sample bytes, and bootstrap limit must be non-zero",
            ));
        }
        if options.sample_bytes > options.bootstrap_limit {
            return Err(QuarryError::InvalidOption(
                "sample bytes cannot exceed bootstrap limit",
            ));
        }

        let started = Instant::now();
        let open_started = Instant::now();
        let mut file = File::open(path.as_ref())?;
        let file_open = open_started.elapsed();
        let source_stamp = SourceStamp::from_file(&file)?;
        let file_size = source_stamp.file_size();
        let encoding = Encoding::detect(&mut file, requested_encoding)?;
        if encoding.is_utf16() && options.bootstrap_limit < 2 {
            return Err(QuarryError::InvalidOption(
                "UTF-16 bootstrap limit must fit a code unit",
            ));
        }

        let delimiter = match options.delimiter {
            Some(delimiter) => {
                RecordScanner::new(delimiter, encoding)?;
                delimiter
            }
            None => detect_file_delimiter(
                &mut file,
                file_size,
                options.sample_bytes,
                options.bootstrap_limit,
                encoding,
            )?,
        };

        file.seek(SeekFrom::Start(0))?;
        let (bootstrap, ends) = read_initial_records(
            &mut file,
            file_size,
            delimiter,
            options.rows,
            options.bootstrap_limit,
            encoding,
        )?;
        let first_rows = materialize_rows(
            &bootstrap,
            &ends[..ends.len().min(options.rows)],
            delimiter,
            encoding,
        )?;
        let has_header = match options.header_mode {
            HeaderMode::Auto => detect_header(&first_rows),
            HeaderMode::FirstRow => true,
            HeaderMode::NoHeader => false,
        };

        Ok(Self {
            path: path.as_ref().to_path_buf(),
            source_stamp,
            encoding,
            completed_record_count: Arc::new(Mutex::new(None)),
            file_size,
            dialect: Dialect {
                delimiter,
                has_header,
            },
            first_rows,
            metrics: OpenMetrics {
                file_open,
                first_rows: started.elapsed(),
                bootstrap_bytes_read: bootstrap.len() as u64,
            },
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    pub fn start_indexing(&self, config: IndexConfig) -> Result<IndexJob, QuarryError> {
        IndexJob::start(self, config)
    }

    fn completed_record_count(&self) -> Option<u64> {
        self.completed_record_count
            .lock()
            .unwrap()
            .filter(|count| {
                count.delimiter == self.dialect.delimiter
                    && count.encoding == self.encoding
                    && count.file_size == self.file_size
            })
            .map(|count| count.records)
    }

    pub fn read_rows(
        &self,
        index: &StructuralIndex,
        start: u64,
        count: usize,
    ) -> Result<Vec<Row>, QuarryError> {
        read_rows_from_index(
            &self.path,
            self.dialect.delimiter,
            index,
            start,
            count,
            DEFAULT_MAX_RECORD_BYTES,
            self.encoding,
        )
    }
}

fn read_initial_records(
    file: &mut (impl Read + Seek),
    file_size: u64,
    delimiter: u8,
    wanted: usize,
    limit: usize,
    encoding: Encoding,
) -> Result<(Vec<u8>, Vec<u64>), QuarryError> {
    let mut scanner = RecordScanner::new(delimiter, encoding)?;
    let mut bytes = Vec::with_capacity(DEFAULT_READ_CHUNK.min(limit));
    let mut chunk = vec![0; encoding.chunk_size(DEFAULT_READ_CHUNK.min(limit))];
    let mut ends = Vec::with_capacity(wanted);

    while ends.len() < wanted && bytes.len() < limit {
        let mut allowed = (limit - bytes.len()).min(chunk.len());
        if encoding.is_utf16() {
            allowed &= !1;
        }
        if allowed == 0 {
            break;
        }
        let read = encoding.read_chunk(file, &mut chunk[..allowed])?;
        if read == 0 {
            break;
        }
        let absolute_start = bytes.len() as u64;
        bytes.extend_from_slice(&chunk[..read]);
        scanner.scan_chunk(&chunk[..read], absolute_start, |end| {
            if ends.len() < wanted {
                ends.push(end);
            }
        })?;
    }

    let reached_eof = bytes.len() as u64 == file_size;
    if reached_eof && ends.len() < wanted {
        scanner.finish(file_size, |end| ends.push(end))?;
    }
    if !reached_eof && ends.len() < wanted {
        return Err(QuarryError::BootstrapLimitExceeded {
            limit,
            rows_found: ends.len(),
        });
    }
    Ok((bytes, ends))
}

fn materialize_rows(
    bytes: &[u8],
    ends: &[u64],
    delimiter: u8,
    encoding: Encoding,
) -> Result<Vec<Row>, QuarryError> {
    let mut rows = Vec::with_capacity(ends.len());
    let mut start = 0;
    for (physical_row, &end) in ends.iter().enumerate() {
        let end = end as usize;
        let fields =
            parse_encoded_record(&bytes[start..end], delimiter, physical_row as u64, encoding)?
                .into_iter()
                .map(|field| field.into_owned())
                .collect();
        rows.push(Row {
            offset: start as u64,
            fields,
        });
        start = end;
    }
    Ok(rows)
}

fn detect_file_delimiter(
    file: &mut (impl Read + Seek),
    file_size: u64,
    sample_bytes: usize,
    limit: usize,
    encoding: Encoding,
) -> Result<u8, QuarryError> {
    let mut sample = Vec::new();
    let mut target = sample_bytes.min(limit);
    if encoding.is_utf16() {
        target = encoding.chunk_size(target).min(limit & !1);
    }
    loop {
        file.by_ref()
            .take((target - sample.len()) as u64)
            .read_to_end(&mut sample)?;
        let at_eof = sample.len() as u64 == file_size;
        if let Some(delimiter) = detect_delimiter(&sample, at_eof, encoding) {
            return Ok(delimiter);
        }
        if sample.len() >= limit {
            return Err(QuarryError::BootstrapLimitExceeded {
                limit,
                rows_found: 0,
            });
        }
        if sample.len() < target {
            return Err(QuarryError::SourceChanged);
        }
        // A partial first record is not evidence for a one-column comma file.
        // Grow only until a complete record is available, within the caller's cap.
        let next = target.saturating_mul(2).min(limit);
        target = if encoding.is_utf16() { next & !1 } else { next };
        if target <= sample.len() {
            return Err(QuarryError::BootstrapLimitExceeded {
                limit,
                rows_found: 0,
            });
        }
    }
}

fn detect_delimiter(sample: &[u8], at_eof: bool, encoding: Encoding) -> Option<u8> {
    let mut best = (0_usize, 0_usize, b',');
    let mut has_record = false;
    for delimiter in *b",\t|;" {
        let Ok(mut scanner) = RecordScanner::new(delimiter, encoding) else {
            continue;
        };
        let mut ends = Vec::new();
        if scanner
            .scan_chunk(sample, 0, |end| {
                if ends.len() < 32 {
                    ends.push(end);
                }
            })
            .is_err()
        {
            continue;
        }
        // Only a complete sample can finish the last record. A bounded sample
        // may end in the middle of a field or quoted record.
        if at_eof
            && scanner
                .finish(sample.len() as u64, |end| {
                    if ends.len() < 32 {
                        ends.push(end);
                    }
                })
                .is_err()
        {
            continue;
        }

        has_record |= !ends.is_empty();

        let mut start = 0;
        let mut frequencies = HashMap::new();
        for (physical_row, end) in ends.into_iter().enumerate() {
            let end = end as usize;
            let Ok(fields) = parse_encoded_record(
                &sample[start..end],
                delimiter,
                physical_row as u64,
                encoding,
            ) else {
                frequencies.clear();
                break;
            };
            *frequencies.entry(fields.len()).or_insert(0_usize) += 1;
            start = end;
        }
        let Some((columns, frequency)) = frequencies
            .into_iter()
            .filter(|(columns, _)| *columns > 1)
            .max_by_key(|(columns, frequency)| (*frequency, *columns))
        else {
            continue;
        };
        if (frequency, columns) > (best.0, best.1) {
            best = (frequency, columns, delimiter);
        }
    }
    (at_eof || has_record).then_some(best.2)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CellKind {
    Empty,
    Number,
    Text,
}

fn detect_header(rows: &[Row]) -> bool {
    let Some((first, rest)) = rows.split_first() else {
        return false;
    };
    let Some(second) = rest.first() else {
        return false;
    };
    if first.fields.len() != second.fields.len() || first.fields.is_empty() {
        return false;
    }
    let unique: HashSet<&[u8]> = first.fields.iter().map(Vec::as_slice).collect();
    if unique.len() != first.fields.len() || first.fields.iter().any(Vec::is_empty) {
        return false;
    }
    let differences = first
        .fields
        .iter()
        .zip(&second.fields)
        .filter(|(left, right)| cell_kind(left) != cell_kind(right))
        .count();
    if differences >= (first.fields.len() / 4).max(1) {
        return true;
    }

    first
        .fields
        .iter()
        .all(|field| looks_like_column_name(field))
        && second
            .fields
            .iter()
            .filter(|field| looks_like_column_name(field))
            .count()
            * 4
            < second.fields.len() * 3
}

fn looks_like_column_name(value: &[u8]) -> bool {
    value
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn cell_kind(value: &[u8]) -> CellKind {
    if value.is_empty() {
        return CellKind::Empty;
    }
    if value
        .iter()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E'))
        && value.iter().any(u8::is_ascii_digit)
    {
        CellKind::Number
    } else {
        CellKind::Text
    }
}

fn read_rows_from_index(
    path: &Path,
    delimiter: u8,
    index: &StructuralIndex,
    start: u64,
    count: usize,
    max_record_bytes: usize,
    encoding: Encoding,
) -> Result<Vec<Row>, QuarryError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if start >= index.indexed_rows() {
        return Err(QuarryError::RowNotIndexed {
            requested: start,
            indexed_rows: index.indexed_rows(),
        });
    }

    let target_end = start.saturating_add(count as u64).min(index.indexed_rows());
    let checkpoint = index.nearest_checkpoint(start);
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(checkpoint.offset))?;
    let mut scanner = RecordScanner::at_offset(delimiter, checkpoint.offset, encoding)?;
    let mut chunk = vec![0; encoding.chunk_size(DEFAULT_READ_CHUNK)];
    let mut absolute_start = checkpoint.offset;
    let mut row_number = checkpoint.row;
    let mut record_start = checkpoint.offset;
    let mut record = Vec::new();
    let mut rows = Vec::with_capacity(count);

    while row_number < target_end {
        let read = encoding.read_chunk(&mut file, &mut chunk)?;
        if read == 0 {
            if !record.is_empty() && row_number >= start && row_number < target_end {
                let fields = parse_encoded_record(&record, delimiter, row_number, encoding)?
                    .into_iter()
                    .map(|field| field.into_owned())
                    .collect();
                rows.push(Row {
                    offset: record_start,
                    fields,
                });
            }
            break;
        }
        let mut segment_start = 0;
        let mut deferred_error = None;
        scanner.scan_chunk(&chunk[..read], absolute_start, |absolute_end| {
            let local_end = (absolute_end - absolute_start) as usize;
            if row_number >= start && row_number < target_end {
                record.extend_from_slice(&chunk[segment_start..local_end]);
                if record.len() > max_record_bytes {
                    deferred_error = Some(QuarryError::RecordTooLarge {
                        limit: max_record_bytes,
                    });
                } else if deferred_error.is_none() {
                    match parse_encoded_record(&record, delimiter, row_number, encoding) {
                        Ok(fields) => rows.push(Row {
                            offset: record_start,
                            fields: fields.into_iter().map(|field| field.into_owned()).collect(),
                        }),
                        Err(error) => deferred_error = Some(error),
                    }
                }
                record.clear();
            }
            row_number += 1;
            record_start = absolute_end;
            segment_start = local_end;
        })?;
        if let Some(error) = deferred_error {
            return Err(error);
        }
        if row_number >= target_end {
            break;
        }
        if row_number >= start {
            record.extend_from_slice(&chunk[segment_start..read]);
            if record.len() > max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: max_record_bytes,
                });
            }
        }
        absolute_start += read as u64;
    }

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{HeaderMode, IndexConfig, OpenOptions, QuarryError, Session};

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture(bytes: &[u8]) -> std::path::PathBuf {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("quarry-{}-{id}.csv", std::process::id()));
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    #[test]
    fn opens_before_indexing_and_navigates_from_checkpoints() {
        let path = fixture(b"name,value\r\na,1\r\n\"multi\nline\",2\r\nc,3");
        let session = Session::open(
            &path,
            OpenOptions {
                rows: 2,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(session.dialect.delimiter, b',');
        assert!(session.dialect.has_header);
        assert_eq!(session.first_rows.len(), 2);

        let index = session
            .start_indexing(IndexConfig {
                chunk_bytes: 7,
                checkpoint_every: 2,
                memory_budget_bytes: 64,
            })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(index.indexed_rows(), 4);
        let rows = session.read_rows(&index, 2, 2).unwrap();
        assert_eq!(rows[0].fields[0], b"multi\nline");
        assert_eq!(rows[1].fields[0], b"c");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn opens_and_reads_a_bom_prefixed_quoted_multiline_first_record() {
        let path = fixture(b"\xEF\xBB\xBF\"one\ncontinued\",two\nlast,row\n");
        let session = Session::open(
            &path,
            OpenOptions {
                rows: 2,
                delimiter: Some(b','),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(session.first_rows[0].fields[0], b"one\ncontinued");
        assert_eq!(session.first_rows[0].fields[1], b"two");

        let index = session
            .start_indexing(IndexConfig {
                chunk_bytes: 1,
                checkpoint_every: 1,
                memory_budget_bytes: 64,
            })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(index.indexed_rows(), 2);
        let rows = session.read_rows(&index, 0, 2).unwrap();
        assert_eq!(rows[0].fields[0], b"one\ncontinued");
        assert_eq!(rows[1].fields, [b"last".to_vec(), b"row".to_vec()]);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn starting_indexing_reports_a_missing_file_synchronously() {
        let path = fixture(b"name,value\na,1\n");
        let session = Session::open(&path, OpenOptions::default()).unwrap();
        fs::remove_file(path).unwrap();

        assert!(matches!(
            session.start_indexing(IndexConfig::default()),
            Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn reads_live_index_and_cancels_promptly() {
        let path = fixture(&b"a,b\n".repeat(250_000));
        let session = Session::open(&path, OpenOptions::default()).unwrap();
        let job = session
            .start_indexing(IndexConfig {
                chunk_bytes: 1,
                checkpoint_every: 8,
                memory_budget_bytes: 64 * 1024,
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let progress = loop {
            let progress = job.progress();
            if progress.rows_scanned >= 100 || progress.done {
                break progress;
            }
            assert!(Instant::now() < deadline, "index did not make progress");
            thread::yield_now();
        };
        assert!(!progress.done, "test file indexed before the live read");

        let snapshot = job.snapshot();
        let rows = session.read_rows(&snapshot, 50, 3).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.fields == [b"a", b"b"]));

        let requested = snapshot.indexed_rows() + 1_000;
        assert!(matches!(
            session.read_rows(&snapshot, requested, 1),
            Err(QuarryError::RowNotIndexed {
                requested: error_row,
                indexed_rows,
            }) if error_row == requested && indexed_rows == snapshot.indexed_rows()
        ));

        let cancel_started = Instant::now();
        job.cancel();
        let index = job.wait().unwrap();
        assert!(cancel_started.elapsed() < Duration::from_secs(1));
        assert!(index.indexed_bytes() < session.file_size);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn applies_delimiter_and_header_overrides() {
        let path = fixture(b"1\t2\n3\t4\n");
        let session = Session::open(
            &path,
            OpenOptions {
                delimiter: Some(b'\t'),
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(session.dialect.delimiter, b'\t');
        assert!(session.dialect.has_header);
        fs::remove_file(path).unwrap();

        let path = fixture(b"name;value\na;1\n");
        let session = Session::open(
            &path,
            OpenOptions {
                delimiter: Some(b';'),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(session.dialect.delimiter, b';');
        assert!(!session.dialect.has_header);
        assert_eq!(
            session.first_rows[0].fields,
            [b"name".to_vec(), b"value".to_vec()]
        );
        fs::remove_file(path).unwrap();

        let path = fixture(b"name|value\na|1\n");
        let session = Session::open(
            &path,
            OpenOptions {
                delimiter: Some(b'|'),
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(session.dialect.delimiter, b'|');
        assert_eq!(session.first_rows[1].fields[0], b"a");
        assert_eq!(session.first_rows[1].fields[1], b"1");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn detects_tab_delimiter_and_bounds_bootstrap() {
        let path = fixture(b"a\tb\n1\t2\n");
        let session = Session::open(&path, OpenOptions::default()).unwrap();
        assert_eq!(session.dialect.delimiter, b'\t');
        fs::remove_file(path).unwrap();

        let path = fixture(&vec![b'x'; 1024]);
        let error = Session::open(
            &path,
            OpenOptions {
                rows: 2,
                sample_bytes: 32,
                bootstrap_limit: 64,
                delimiter: Some(b','),
                header_mode: HeaderMode::Auto,
            },
        )
        .unwrap_err();
        assert!(matches!(error, QuarryError::BootstrapLimitExceeded { .. }));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn grows_delimiter_sample_without_exceeding_bootstrap_limit() {
        let path = fixture(format!("ID\t{}\n", "x".repeat(100)).as_bytes());
        let session = Session::open(
            &path,
            OpenOptions {
                rows: 1,
                sample_bytes: 16,
                bootstrap_limit: 128,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(session.dialect.delimiter, b'\t');
        assert_eq!(session.first_rows[0].fields.len(), 2);
        assert!(matches!(
            Session::open(
                &path,
                OpenOptions {
                    rows: 1,
                    sample_bytes: 16,
                    bootstrap_limit: 64,
                    ..OpenOptions::default()
                },
            ),
            Err(QuarryError::BootstrapLimitExceeded {
                limit: 64,
                rows_found: 0,
            })
        ));
        fs::remove_file(path).unwrap();
    }
}

mod working_storage;
pub use working_storage::{WorkingCopyCleanup, WorkingCopyStorage};
