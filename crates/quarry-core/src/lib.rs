mod case;
mod cleanup;
mod column_discovery;
mod combine;
mod compressed;
mod date_time_sort;
mod destination;
mod encoding;
mod export;
mod filter;
mod index;
mod inspection;
mod join;
#[cfg(test)]
mod join_faults;
mod pool;
mod positional_io;
mod search;
mod section;
mod sidecar;
mod sort;
mod source_stamp;
mod spill;
mod splice;
mod storage;
mod watch;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
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
pub use column_discovery::ColumnDiscoveryJob;
pub use combine::{
    CombineInput, CombineJob, CombineOptions, CombinePlan, CombineProgress, CombineSummary,
    check_combine_files, check_combine_files_with_encoding,
};
/// Debug-build checkpoints for tests of decompression workers; absent in release builds.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub use compressed::test_pause as compressed_test_pause;
pub use compressed::{
    CompressedBootstrap, CompressedDecompressJob, CompressedOpenConfig, CompressedOpenOutcome,
    CompressedProgress, CompressedRawDecompressJob, CompressedSourceGuard, CompressionFormat,
    DecompressedCopy, detect_compression, is_compressed_destination, reject_compressed_destination,
};
pub use date_time_sort::{
    DEFAULT_DATE_FORMAT_SUGGESTION_MAX_BYTES, DEFAULT_DATE_FORMAT_SUGGESTION_MAX_ROWS,
    DateFormatSuggestion, DateFormatSuggestionConfig, DateTimeFormat,
    suggest_date_time_formats_from_values,
};
pub use encoding::Encoding;
use encoding::RecordScanner;
pub use export::{
    CleanupJob, CleanupOutcome, CleanupSummary, ColumnTransformation, FilterExportJob,
    FilterExportOutcome, FilterExportProgress, FilterExportSummary, LiteralReplacement,
    MAX_TRANSFORMATION_COLUMNS, ReplaceAllJob, ReplaceAllOptions, ReplaceAllOutcome,
    ReplaceAllSummary, RowInsertion, SaveAsJob, SaveAsOutcome, SaveAsProgress, SaveAsSummary,
    SplitAnalysisJob, SplitAnalysisOutcome, SplitAnalysisProgress, SplitAnalysisSummary,
};
pub use filter::{
    FilterEdits, FilterIndex, FilterJob, FilterMatch, FilterOperator, FilterPredicate,
    FilterProgress, FilterQuery, FilterReadJob, FilterReadOutcome, FilterReadProgress,
};
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub use index::test_hooks;
pub use index::{
    Checkpoint, CompletedIndex, IndexConfig, IndexJob, IndexProgress, IndexTelemetry,
    StructuralIndex,
};
pub use inspection::{
    Diagnostic, DiagnosticKind, InspectionEdits, InspectionJob, InspectionOptions,
    InspectionProgress, InspectionReport, InspectionStatus, RecoveryJob, RecoverySummary,
    SourcePreview,
};
pub use join::{
    JoinAnalysis, JoinCase, JoinConfig, JoinDuplicateExample, JoinFormat, JoinHeader, JoinInput,
    JoinInputInfo, JoinJob, JoinMode, JoinPhase, JoinPlan, JoinProgress, JoinSourceVersion,
    JoinSummary, JoinUnmatchedExample, analyze_join,
};
pub use join::{
    JoinCellEdits, JoinHeaderEdits, JoinInputOwner, JoinWorkingSource, analyze_join_with_working,
    read_join_working_header,
};
pub use join::{
    JoinOutputColumn, JoinOutputOptions, JoinSource, analyze_join_with_output, read_join_header,
    read_join_header_with_format,
};
pub use pool::{
    DEFAULT_SINGLE_RUN_MEMORY_BUDGET_BYTES, NofileLimitStatus, ProcessMemorySample,
    default_single_run_memory_budget_bytes, default_worker_threads, process_memory_usage,
    raise_nofile_limit,
};
use quarry_delimited::ParseError;
pub use quarry_delimited::{
    ByteEncodingClass, EscapeRule, ScannerDialect, SimdBackend, active_simd_backend,
};
pub use search::{
    SearchJob, SearchMatch, SearchOutcome, SearchPosition, SearchProgress, SearchTelemetry,
};
pub use section::{
    BoundaryProof, BoundaryTrust, ByteUnit, ByteWindow, MAX_SPECULATIVE_SEARCH_BYTES,
    SectionBounds, SectionOpenOptions, VerifyBoundaryJob, VerifyBoundaryOutcome,
    VerifyBoundaryProgress, format_exact_bytes_in_unit, parse_exact_byte_input,
};
pub use sidecar::{
    CacheClearSummary, DEFAULT_INDEX_CACHE_MAX_BYTES, DEFAULT_INDEX_CACHE_MIN_FILE_BYTES,
    IndexCacheConfig, clear_index_cache, default_index_cache_dir,
};
pub use sort::{
    DuplicateJob, DuplicateOptions, DuplicateOutcome, DuplicateProgress, DuplicateSpec,
    DuplicateSummary, DuplicateTarget, MAX_SORT_KEYS, SortDirection, SortJob, SortMode,
    SortOptions, SortOutcome, SortPlan, SortProgress, SortSpec, SortSummary,
    estimate_multi_sort_temporary_bytes, estimate_sort_plan_temporary_bytes,
    estimate_sort_temporary_bytes,
};
pub use source_stamp::{
    ContainedDir, FileIdentity, SourceGuard, open_contained_regular_file,
    open_regular_file_nofollow,
};
#[cfg(target_os = "macos")]
pub use source_stamp::{acquire_macos_install_lock, acquire_macos_install_lock_at};
pub use splice::{SpliceDiskPreflight, SpliceSaveSummary, SpliceStrategy};
pub use storage::{
    StorageSpace, available_disk_space, check_storage, default_temp_dir,
    estimate_duplicate_target_temporary_bytes, estimate_duplicate_temporary_bytes,
    estimate_edited_output_bytes, estimate_edited_output_bytes_from_values, inspect_storage,
    restrict_private_file, validate_scratch_directory,
};
pub use watch::{
    DEFAULT_WATCH_POLL_INTERVAL, DirectoryWatch, DirectoryWatchEvent, DirectoryWatchWakeup,
    LiveWatchFilterJob, LiveWatchJob, LiveWatchReader, NETWORK_WATCH_POLL_INTERVAL, WatchAlert,
    WatchBackend, WatchConfig, WatchProvisionalTail, WatchSnapshot, WatchStatus, WatchStopOutcome,
};

const DEFAULT_SAMPLE_BYTES: usize = 1024 * 1024;
const DEFAULT_BOOTSTRAP_LIMIT: usize = 64 * 1024 * 1024;
const DEFAULT_READ_CHUNK: usize = 1024 * 1024;
const DEFAULT_MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// Parses native record bytes and wraps record-local decoding failures with their logical record.
pub(crate) fn parse_encoded_record(
    record: &[u8],
    delimiter: u8,
    physical_row: u64,
    encoding: Encoding,
) -> Result<Vec<Cow<'_, [u8]>>, QuarryError> {
    parse_encoded_record_with_bom(record, delimiter, physical_row, physical_row == 0, encoding)
}

/// Parses native record bytes with explicit control over whether a leading BOM should be stripped
/// (only true for a record starting at byte 0 of the source file, never for interior sections).
pub(crate) fn parse_encoded_record_with_bom(
    record: &[u8],
    delimiter: u8,
    physical_row: u64,
    strip_bom: bool,
    encoding: Encoding,
) -> Result<Vec<Cow<'_, [u8]>>, QuarryError> {
    parse_encoded_record_with_bom_and_field_limit(
        record,
        delimiter,
        physical_row,
        strip_bom,
        encoding,
        usize::MAX,
    )
}

/// Parses native record bytes with an explicit upper bound on materialized fields so hostile
/// delimiter-only records cannot amplify heap allocations beyond `max_fields`.
pub(crate) fn parse_encoded_record_with_bom_and_field_limit(
    record: &[u8],
    delimiter: u8,
    physical_row: u64,
    strip_bom: bool,
    encoding: Encoding,
    max_fields: usize,
) -> Result<Vec<Cow<'_, [u8]>>, QuarryError> {
    let body = if strip_bom {
        encoding.strip_bom(record).1
    } else {
        record
    };
    if encoding == Encoding::Utf8 {
        return quarry_delimited::parse_record_with_field_limit(body, delimiter, max_fields)
            .map_err(Into::into);
    }
    let decoded = encoding
        .decode(body, DEFAULT_MAX_RECORD_BYTES)
        .map_err(|error| record_encoding_error(physical_row.saturating_add(1), error))?;
    match decoded {
        Cow::Borrowed(bytes) => {
            quarry_delimited::parse_record_with_field_limit(bytes, delimiter, max_fields)
                .map_err(Into::into)
        }
        Cow::Owned(bytes) => Ok(quarry_delimited::parse_record_with_field_limit(
            &bytes, delimiter, max_fields,
        )?
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
    /// A compressed input that Quarry cannot read: unsupported, corrupt or truncated.
    CompressedFile {
        path: PathBuf,
        reason: String,
    },
    DecompressionLimitExceeded {
        path: PathBuf,
        limit_bytes: u64,
        decompressed_bytes: u64,
    },
    /// Compressed documents are read-only; write an uncompressed copy instead.
    CompressedReadOnly,
    /// Section boundary snapping could not locate a record terminator within the bounded search cap.
    SectionBoundaryNotFound {
        search_limit_bytes: usize,
        reason: String,
    },
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
            Self::CompressedFile { path, reason } => write!(f, "{}: {reason}", path.display()),
            Self::DecompressionLimitExceeded {
                path,
                limit_bytes,
                decompressed_bytes,
            } => write!(
                f,
                "{}: decompressed data exceeds the {limit_bytes}-byte limit ({decompressed_bytes} bytes reached). No partial copy was kept; raise the limit or use a smaller file.",
                path.display()
            ),
            Self::CompressedReadOnly => write!(
                f,
                "Compressed files open read-only. Use Save Copy As to write an uncompressed copy; the compressed file is unchanged."
            ),
            Self::SectionBoundaryNotFound {
                search_limit_bytes,
                reason,
            } => write!(
                f,
                "{reason} (search limit: {search_limit_bytes} bytes). Try a different byte range or run Verify Boundary."
            ),
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
    /// Present when this document reads a private copy of a compressed source.
    compressed: Option<Arc<compressed::CompressedBacking>>,
    /// Present when this document views a bounded byte section of a parent file.
    section: Option<section::SectionMetadata>,
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
    /// Compressed sources are rejected; open them with `CompressedBootstrap`.
    pub fn open_with_encoding(
        path: impl AsRef<Path>,
        options: OpenOptions,
        requested_encoding: Option<Encoding>,
    ) -> Result<Self, QuarryError> {
        Self::open_checked(path.as_ref(), options, requested_encoding, true)
    }

    /// Open a private decompressed copy, which is plain text by construction.
    pub(crate) fn open_uncompressed(
        path: &Path,
        options: OpenOptions,
        requested_encoding: Option<Encoding>,
    ) -> Result<Self, QuarryError> {
        Self::open_checked(path, options, requested_encoding, false)
    }

    fn open_checked(
        path: &Path,
        options: OpenOptions,
        requested_encoding: Option<Encoding>,
        reject_compressed: bool,
    ) -> Result<Self, QuarryError> {
        validate_open_options(&options)?;

        let started = Instant::now();
        let open_started = Instant::now();
        let mut file = crate::source_stamp::open_regular_file(path)?;
        let file_open = open_started.elapsed();
        if reject_compressed && let Some(format) = compressed::compression_of(&mut file, path)? {
            return Err(QuarryError::CompressedFile {
                path: path.to_path_buf(),
                reason: format!(
                    "This file is {format}-compressed. Open it as a compressed file so Quarry can decompress it safely."
                ),
            });
        }
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
                Some(file_size),
                options.sample_bytes,
                options.bootstrap_limit,
                encoding,
            )?,
        };

        file.seek(SeekFrom::Start(0))?;
        let (bootstrap, ends) = read_initial_records(
            &mut file,
            Some(file_size),
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
            path: path.to_path_buf(),
            source_stamp,
            encoding,
            completed_record_count: Arc::new(Mutex::new(None)),
            compressed: None,
            section: None,
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

    /// Open a bounded byte section `[window.base, window.base + window.len)` of a regular
    /// uncompressed delimited file with outward record-boundary snapping and trust metadata.
    pub fn open_section(
        path: impl AsRef<Path>,
        window: ByteWindow,
        options: SectionOpenOptions,
    ) -> Result<Self, QuarryError> {
        Self::open_section_inner(path.as_ref(), window, options, None)
    }

    /// Reopens a bounded byte section at previously validated `preserved_bounds` ONLY when the
    /// underlying file still matches `expected_guard` AND the resolved `(delimiter, encoding)`
    /// match `(expected_delimiter, expected_encoding)`. Returns `QuarryError::SourceChanged` if
    /// the file changed on disk, or re-snaps `preserved_bounds.requested` when the resolved
    /// delimiter or encoding changed. Used by verified boundary reopen and clean-section dormancy
    /// wake so proven bounds (including multiline quoted starts past 16 MiB) are never re-snapped
    /// speculatively when file and framing are unchanged.
    pub fn open_section_preserved(
        path: impl AsRef<Path>,
        preserved_bounds: SectionBounds,
        expected_guard: &SourceGuard,
        expected_delimiter: u8,
        expected_encoding: Encoding,
        options: SectionOpenOptions,
    ) -> Result<Self, QuarryError> {
        Self::open_section_inner(
            path.as_ref(),
            preserved_bounds.requested,
            options,
            Some((
                preserved_bounds,
                expected_guard,
                expected_delimiter,
                expected_encoding,
            )),
        )
    }

    fn open_section_inner(
        path: &Path,
        window: ByteWindow,
        options: SectionOpenOptions,
        preserved: Option<(SectionBounds, &SourceGuard, u8, Encoding)>,
    ) -> Result<Self, QuarryError> {
        validate_open_options(&options.open)?;
        if window.end().is_none() {
            return Err(QuarryError::InvalidOption(
                "section byte window overflows 64-bit range",
            ));
        }

        let started = Instant::now();
        let open_started = Instant::now();
        let mut file = crate::source_stamp::open_regular_file(path)?;
        let file_open = open_started.elapsed();
        if let Some(format) = compressed::compression_of(&mut file, path)? {
            return Err(QuarryError::CompressedFile {
                path: path.to_path_buf(),
                reason: format!(
                    "This file is {format}-compressed. Open Section requires an uncompressed file; decompress or save an uncompressed copy first."
                ),
            });
        }
        let source_stamp = SourceStamp::from_file(&file)?;
        if let Some((_, expected_guard, _, _)) = preserved
            && SourceGuard::from_stamp(source_stamp.clone()) != *expected_guard
        {
            return Err(QuarryError::SourceChanged);
        }
        let file_size = source_stamp.file_size();
        let encoding = Encoding::detect(&mut file, options.encoding)?;
        if encoding.is_utf16() && options.open.bootstrap_limit < 2 {
            return Err(QuarryError::InvalidOption(
                "UTF-16 bootstrap limit must fit a code unit",
            ));
        }

        let bom = encoding.bom();
        let bom_len = if !bom.is_empty() && file_size >= bom.len() as u64 {
            let mut probe = [0u8; 3];
            let read = positional_io::read_at_exact_or_eof(&file, 0, &mut probe[..bom.len()])?;
            if read == bom.len() && &probe[..bom.len()] == bom {
                bom.len() as u64
            } else {
                0
            }
        } else {
            0
        };

        file.seek(SeekFrom::Start(0))?;
        let delimiter = match options.open.delimiter {
            Some(delimiter) => {
                RecordScanner::new(delimiter, encoding)?;
                delimiter
            }
            None => detect_file_delimiter(
                &mut file,
                Some(file_size),
                options.open.sample_bytes,
                options.open.bootstrap_limit,
                encoding,
            )?,
        };

        // Read only as many rows from byte 0 as needed to resolve headers (0 for NoHeader,
        // 1 for FirstRow, 2 for Auto header detection), stopping immediately at the closing
        // record boundary so malformed CSV later in the file never breaks opening a valid section.
        let (has_header, header_row, byte0_bytes_read) = match options.open.header_mode {
            HeaderMode::NoHeader => (false, None, 0u64),
            HeaderMode::FirstRow => {
                let (byte0_rows, bytes_read, reached_eof) = section::read_header_probe_rows(
                    &file,
                    file_size,
                    delimiter,
                    1,
                    options.open.bootstrap_limit,
                    encoding,
                )?;
                if byte0_rows.is_empty() && !reached_eof {
                    return Err(QuarryError::BootstrapLimitExceeded {
                        limit: options.open.bootstrap_limit,
                        rows_found: 0,
                    });
                }
                (true, byte0_rows.into_iter().next(), bytes_read)
            }
            HeaderMode::Auto => {
                let (byte0_rows, bytes_read, reached_eof) = section::read_header_probe_rows(
                    &file,
                    file_size,
                    delimiter,
                    2,
                    options.open.bootstrap_limit,
                    encoding,
                )?;
                if byte0_rows.len() < 2 && !reached_eof {
                    return Err(QuarryError::BootstrapLimitExceeded {
                        limit: options.open.bootstrap_limit,
                        rows_found: byte0_rows.len(),
                    });
                }
                let detected = detect_header(&byte0_rows);
                let header = if detected {
                    byte0_rows.into_iter().next()
                } else {
                    None
                };
                (detected, header, bytes_read)
            }
        };

        let bounds = if let Some((preserved_bounds, _, expected_delimiter, expected_encoding)) =
            preserved
            && delimiter == expected_delimiter
            && encoding == expected_encoding
        {
            if preserved_bounds.snapped.end().is_none()
                || preserved_bounds.snapped_end() > file_size
            {
                return Err(QuarryError::SourceChanged);
            }
            preserved_bounds
        } else {
            section::snap_section_window(
                &file,
                file_size,
                delimiter,
                encoding,
                bom_len,
                &source_stamp,
                window,
                &options,
            )?
        };
        let (first_rows, section_bytes_read) = section::read_section_initial_rows(
            &file,
            bounds.snapped.base,
            bounds.snapped_end(),
            delimiter,
            options.open.rows,
            options.open.bootstrap_limit,
            encoding,
            bounds.snapped.base == 0,
        )?;
        // Revalidate that the file did not change between the byte-0 header probe and the
        // section initial-row read so header, stamp, bounds, and rows share one generation.
        if !export::source_matches_stamp(&file, path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }

        Ok(Self {
            path: path.to_path_buf(),
            source_stamp,
            encoding,
            completed_record_count: Arc::new(Mutex::new(None)),
            compressed: None,
            section: Some(section::SectionMetadata {
                bounds,
                header_row,
                bom_len,
                backed_by_working_copy: false,
                origin_parent_stamp: None,
            }),
            file_size,
            dialect: Dialect {
                delimiter,
                has_header,
            },
            first_rows,
            metrics: OpenMetrics {
                file_open,
                first_rows: started.elapsed(),
                bootstrap_bytes_read: byte0_bytes_read.saturating_add(section_bytes_read),
            },
        })
    }

    /// Open a private section-only working copy produced by a section-scoped row edit or
    /// Replace All, preserving the original parent section's bounds, trust, header, and BOM
    /// provenance while reading `[0, file_size)` of `working_path`.
    pub fn open_section_working_copy(
        working_path: impl AsRef<Path>,
        original_parent: &Session,
        options: OpenOptions,
    ) -> Result<Self, QuarryError> {
        validate_open_options(&options)?;
        let Some(parent_meta) = &original_parent.section else {
            return Err(QuarryError::InvalidOption(
                "open_section_working_copy requires an original section session",
            ));
        };
        let path = working_path.as_ref();
        let started = Instant::now();
        let open_started = Instant::now();
        let file = crate::source_stamp::open_regular_file(path)?;
        let file_open = open_started.elapsed();
        let source_stamp = SourceStamp::from_file(&file)?;
        let file_size = source_stamp.file_size();
        let encoding = original_parent.encoding;
        let delimiter = original_parent.dialect.delimiter;
        let strip_bom_at_zero = parent_meta.bounds.snapped.base == 0;
        let (first_rows, section_bytes_read) = section::read_section_initial_rows(
            &file,
            0,
            file_size,
            delimiter,
            options.rows,
            options.bootstrap_limit,
            encoding,
            strip_bom_at_zero,
        )?;
        if !export::source_matches_stamp(&file, path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        Ok(Self {
            path: path.to_path_buf(),
            source_stamp,
            encoding,
            completed_record_count: Arc::new(Mutex::new(None)),
            compressed: None,
            section: Some(section::SectionMetadata {
                bounds: parent_meta.bounds,
                header_row: parent_meta.header_row.clone(),
                bom_len: parent_meta.bom_len,
                backed_by_working_copy: true,
                origin_parent_stamp: Some(original_parent.source_stamp.clone()),
            }),
            file_size,
            dialect: original_parent.dialect,
            first_rows,
            metrics: OpenMetrics {
                file_open,
                first_rows: started.elapsed(),
                bootstrap_bytes_read: section_bytes_read,
            },
        })
    }

    /// Returns section bounds and trust metadata when this session was opened via `open_section`.
    pub fn section_bounds(&self) -> Option<SectionBounds> {
        self.section.as_ref().map(|meta| meta.bounds)
    }

    /// True when this session views a bounded byte section of a parent file.
    pub fn is_section(&self) -> bool {
        self.section.is_some()
    }

    /// True when this session is an interior bounded section (`bounds.snapped.base > 0`), even
    /// when currently reading a section-only private working copy starting at offset 0.
    pub fn section_is_interior(&self) -> bool {
        self.section
            .as_ref()
            .is_some_and(|meta| meta.bounds.snapped.base > 0)
    }

    /// True when this section session reads a private section-only working copy rather than the
    /// parent file directly.
    pub fn section_backed_by_working_copy(&self) -> bool {
        self.section
            .as_ref()
            .is_some_and(|meta| meta.backed_by_working_copy)
    }

    /// Return a new `Session` handle over the same file and generation with `SectionBounds`
    /// upgraded in-place to `BoundaryTrust::Verified` after `VerifyBoundaryJob` confirms the
    /// existing snapped window did not shift.
    pub fn with_verified_section_bounds(
        &self,
        verified: SectionBounds,
    ) -> Result<Self, QuarryError> {
        let Some(meta) = &self.section else {
            return Err(QuarryError::InvalidOption(
                "with_verified_section_bounds requires a bounded section session",
            ));
        };
        if verified.trust != BoundaryTrust::Verified || verified.snapped != meta.bounds.snapped {
            return Err(QuarryError::InvalidOption(
                "verified bounds must be Verified and match the current snapped byte window",
            ));
        }
        let mut next_meta = meta.clone();
        next_meta.bounds = verified;
        Ok(Self {
            path: self.path.clone(),
            source_stamp: self.source_stamp.clone(),
            encoding: self.encoding,
            completed_record_count: Arc::clone(&self.completed_record_count),
            compressed: self.compressed.clone(),
            section: Some(next_meta),
            file_size: self.file_size,
            dialect: self.dialect,
            first_rows: self.first_rows.clone(),
            metrics: self.metrics,
        })
    }

    /// Estimate advisory disk preflight for splicing this `Verified` section back into its parent.
    pub fn estimate_splice_save_storage(
        &self,
        original_parent: Option<&Session>,
        cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    ) -> Result<SpliceDiskPreflight, QuarryError> {
        let parent = original_parent.unwrap_or(self);
        let Some(meta) = &parent.section else {
            return Err(QuarryError::InvalidOption(
                "splice preflight is only available for bounded section documents",
            ));
        };
        if meta.bounds.trust != BoundaryTrust::Verified {
            return Err(QuarryError::InvalidOption(
                "Verify section boundaries before saving changes back to the parent file.",
            ));
        }
        let (read_base, read_end) = self.active_byte_range();
        splice::estimate_splice_preflight(
            &parent.path,
            parent.file_size,
            meta.bounds.snapped.base,
            meta.bounds.snapped_end(),
            read_end.saturating_sub(read_base),
            cell_edits,
        )
    }

    /// Returns the byte-0 header row captured when opening a section of a headered file.
    pub fn section_header_row(&self) -> Option<&Row> {
        self.section
            .as_ref()
            .and_then(|meta| meta.header_row.as_ref())
    }

    /// Active half-open byte range `[start, end)` viewed by this session (`[0, file_size)` for
    /// whole files and section working copies, `[snapped.base, snapped_end)` for direct sections).
    pub fn active_byte_range(&self) -> (u64, u64) {
        self.section.as_ref().map_or((0, self.file_size), |meta| {
            if meta.backed_by_working_copy {
                (0, self.file_size)
            } else {
                (meta.bounds.snapped.base, meta.bounds.snapped_end())
            }
        })
    }

    /// Length in bytes of the active byte range viewed by this session.
    pub fn active_byte_len(&self) -> u64 {
        let (start, end) = self.active_byte_range();
        end.saturating_sub(start)
    }

    /// Start a pausable, cancellable background `VerifyBoundaryJob` that proves exact record
    /// boundaries and global row ordinals for this section under `SourceStamp` guards.
    pub fn start_verify_section_boundary(
        &self,
        parent_index: Option<&StructuralIndex>,
    ) -> Result<VerifyBoundaryJob, QuarryError> {
        let Some(meta) = &self.section else {
            return Err(QuarryError::InvalidOption(
                "Verify Boundary is only available for bounded section documents",
            ));
        };
        if meta.backed_by_working_copy {
            return Err(QuarryError::InvalidOption(
                "Save or discard structural section edits before verifying section boundaries.",
            ));
        }
        VerifyBoundaryJob::start(
            self.path.clone(),
            self.source_stamp.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.encoding,
            meta.bounds,
            parent_index,
        )
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Captures a lightweight source-version guard (no open file handle) for dormancy checks.
    pub fn source_guard(&self) -> SourceGuard {
        SourceGuard::from_stamp(self.source_stamp.clone())
    }

    /// Returns OS-level file identity (`(dev, ino)` on Unix) without holding an open descriptor.
    pub fn file_identity(&self) -> Option<FileIdentity> {
        self.source_stamp.file_identity()
    }

    pub fn start_indexing(&self, config: IndexConfig) -> Result<IndexJob, QuarryError> {
        IndexJob::start(self, config, None)
    }

    pub fn start_indexing_with_threads(
        &self,
        config: IndexConfig,
        threads: Option<usize>,
    ) -> Result<IndexJob, QuarryError> {
        IndexJob::start(self, config, threads)
    }

    pub fn start_indexing_with_options(
        &self,
        config: IndexConfig,
        threads: Option<usize>,
        cache_config: IndexCacheConfig,
    ) -> Result<IndexJob, QuarryError> {
        IndexJob::start_with_options(self, config, threads, cache_config)
    }

    /// Starts opt-in append-only Live Watch Mode (ADR 0016) on a clean, uncompressed, whole-file
    /// session whose initial [`StructuralIndex`] has already reached validated EOF.
    pub fn start_live_watch(
        &self,
        index: &StructuralIndex,
        active_filter: Option<&FilterIndex>,
        config: WatchConfig,
    ) -> Result<LiveWatchJob, QuarryError> {
        LiveWatchJob::start(self, index, active_filter, config)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_watch_stop_parts(
        path: PathBuf,
        encoding: Encoding,
        dialect: Dialect,
        metrics: OpenMetrics,
        open_file: &File,
        final_stamp: SourceStamp,
        final_index: &StructuralIndex,
        reuse_first_rows: Option<Vec<Row>>,
    ) -> Result<Self, QuarryError> {
        let file_size = final_stamp.file_size();
        let first_rows = match reuse_first_rows {
            Some(rows) => rows,
            None => read_bootstrap_rows_from_open_file(
                open_file,
                dialect.delimiter,
                encoding,
                final_index.committed_record_end(),
                100,
                DEFAULT_BOOTSTRAP_LIMIT,
            )?,
        };
        let completed = (final_index.is_complete()
            && final_index.committed_record_end() == file_size)
            .then_some(CompletedRecordCount {
                delimiter: dialect.delimiter,
                encoding,
                file_size,
                records: final_index.indexed_rows(),
            });
        let completed_record_count = Arc::new(Mutex::new(completed));
        Ok(Self {
            path,
            source_stamp: final_stamp,
            encoding,
            completed_record_count,
            compressed: None,
            section: None,
            file_size,
            dialect,
            first_rows,
            metrics,
        })
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
        let (section_base, section_end) = self.active_byte_range();
        if !index.matches_session(
            &self.source_stamp,
            self.dialect.delimiter,
            self.encoding,
            section_base,
            section_end,
            self.is_section(),
        ) {
            return Err(QuarryError::InvalidOption(
                "index does not match session source, framing, or byte range",
            ));
        }
        let end = index.section_end().unwrap_or(u64::MAX);
        read_rows_from_index(
            &self.path,
            self.dialect.delimiter,
            index,
            start,
            count,
            DEFAULT_MAX_RECORD_BYTES,
            self.encoding,
            end,
            !self.section_is_interior(),
        )
    }

    /// Resolves the byte offset where logical record `row` starts in this session's active
    /// backing file, returning `Ok(None)` if the row is not yet indexed or past EOF.
    pub fn record_start_offset(
        &self,
        index: Option<&StructuralIndex>,
        row: u64,
    ) -> Result<Option<u64>, QuarryError> {
        if let Ok(first_idx) = usize::try_from(row)
            && let Some(found) = self.first_rows.get(first_idx)
        {
            return Ok(Some(found.offset));
        }
        let Some(index) = index else {
            return Ok(None);
        };
        if row >= index.indexed_rows() {
            return Ok(None);
        }
        let rows = self.read_rows(index, row, 1)?;
        Ok(rows.first().map(|found| found.offset))
    }

    /// Resolves record start byte offsets for a batch of logical rows using a single file open,
    /// one reusable chunk buffer, and checkpoint-grouped scans without parsing field payloads.
    ///
    /// Returns `Ok(None)` promptly when `cancel` becomes `true`, and verifies
    /// [`Self::ensure_source_unchanged`] before and after reading.
    pub fn record_start_offsets(
        &self,
        index: &StructuralIndex,
        rows: &[u64],
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<Vec<(u64, u64)>>, QuarryError> {
        use std::sync::atomic::Ordering;

        if rows.is_empty() {
            return Ok(Some(Vec::new()));
        }
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        self.ensure_source_unchanged()?;
        let (section_base, section_end) = self.active_byte_range();
        if !index.matches_session(
            &self.source_stamp,
            self.dialect.delimiter,
            self.encoding,
            section_base,
            section_end,
            self.is_section(),
        ) {
            return Err(QuarryError::InvalidOption(
                "index does not match session source, framing, or byte range",
            ));
        }

        let mut resolved = Vec::with_capacity(rows.len());
        let mut to_scan = Vec::new();
        let indexed_rows = index.indexed_rows();
        for &row in rows {
            if let Ok(first_idx) = usize::try_from(row)
                && let Some(found) = self.first_rows.get(first_idx)
            {
                resolved.push((row, found.offset));
            } else if row < indexed_rows {
                to_scan.push(row);
            }
        }
        to_scan.sort_unstable();
        to_scan.dedup();
        if to_scan.is_empty() {
            resolved.sort_unstable_by_key(|&(row, _)| row);
            resolved.dedup_by_key(|entry| entry.0);
            return Ok(Some(resolved));
        }

        let end = index.section_end().unwrap_or(u64::MAX);
        let strip_bom_at_zero = !self.section_is_interior();
        let mut file = File::open(&self.path)?;
        let mut chunk = vec![0; self.encoding.chunk_size(DEFAULT_READ_CHUNK)];

        let mut i = 0;
        while i < to_scan.len() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let checkpoint = index.nearest_checkpoint(to_scan[i]);
            let mut group_end = i + 1;
            while group_end < to_scan.len()
                && index.nearest_checkpoint(to_scan[group_end]).row == checkpoint.row
            {
                group_end += 1;
            }
            let targets = &to_scan[i..group_end];
            i = group_end;

            let mut target_idx = 0;
            let mut row_number = checkpoint.row;
            let mut record_start = checkpoint.offset;
            if targets[target_idx] == row_number {
                resolved.push((row_number, record_start));
                target_idx += 1;
                if target_idx >= targets.len() {
                    continue;
                }
            }

            let target_last = *targets.last().expect("non-empty checkpoint group");
            if self.encoding.is_utf16() {
                file.seek(SeekFrom::Start(checkpoint.offset))?;
            }
            let mut scanner = RecordScanner::at_offset_with_bom_handling(
                self.dialect.delimiter,
                checkpoint.offset,
                self.encoding,
                strip_bom_at_zero,
            )?;
            let mut absolute_start = checkpoint.offset;
            let mut has_pending_bytes = false;

            while target_idx < targets.len() && row_number <= target_last {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(None);
                }
                let remaining = end.saturating_sub(absolute_start);
                let read = if remaining == 0 {
                    0
                } else {
                    let limit = chunk
                        .len()
                        .min(usize::try_from(remaining).unwrap_or(usize::MAX));
                    if self.encoding.is_utf16() {
                        self.encoding.read_chunk(&mut file, &mut chunk[..limit])?
                    } else {
                        positional_io::read_at_exact_or_eof(
                            &file,
                            absolute_start,
                            &mut chunk[..limit],
                        )?
                    }
                };
                if read == 0 {
                    if has_pending_bytes
                        && target_idx < targets.len()
                        && targets[target_idx] == row_number
                    {
                        resolved.push((row_number, record_start));
                    }
                    break;
                }
                let mut last_local_end = 0usize;
                scanner.scan_chunk(&chunk[..read], absolute_start, |absolute_end| {
                    let local_end = (absolute_end - absolute_start) as usize;
                    row_number += 1;
                    record_start = absolute_end;
                    last_local_end = local_end;
                    if target_idx < targets.len() && targets[target_idx] == row_number {
                        resolved.push((row_number, record_start));
                        target_idx += 1;
                    }
                })?;
                has_pending_bytes = last_local_end < read;
                absolute_start += read as u64;
            }
        }

        self.ensure_source_unchanged()?;
        resolved.sort_unstable_by_key(|&(row, _)| row);
        resolved.dedup_by_key(|entry| entry.0);
        Ok(Some(resolved))
    }
}

fn validate_open_options(options: &OpenOptions) -> Result<(), QuarryError> {
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
    Ok(())
}

/// `file_size` is None for a stream whose end is known only when a read returns zero.
fn read_initial_records(
    file: &mut (impl Read + Seek),
    file_size: Option<u64>,
    delimiter: u8,
    wanted: usize,
    limit: usize,
    encoding: Encoding,
) -> Result<(Vec<u8>, Vec<u64>), QuarryError> {
    let mut scanner = RecordScanner::new(delimiter, encoding)?;
    let mut bytes = Vec::with_capacity(DEFAULT_READ_CHUNK.min(limit));
    let mut chunk = vec![0; encoding.chunk_size(DEFAULT_READ_CHUNK.min(limit))];
    let mut ends = Vec::with_capacity(wanted);

    let mut stream_ended = false;
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
            stream_ended = true;
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

    // A stream that fills the limit may still end exactly there; one probe byte
    // (never kept) tells it apart from a longer stream.
    if file_size.is_none() && !stream_ended && ends.len() < wanted {
        stream_ended = stream_ends_here(file)?;
    }
    let reached_eof = match file_size {
        Some(file_size) => bytes.len() as u64 == file_size,
        None => stream_ended,
    };
    if reached_eof && ends.len() < wanted {
        scanner.finish(bytes.len() as u64, |end| ends.push(end))?;
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

/// `file_size` is None for a stream; a short sample then marks its end.
fn detect_file_delimiter(
    file: &mut (impl Read + Seek),
    file_size: Option<u64>,
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
        let at_eof = match file_size {
            Some(file_size) => sample.len() as u64 == file_size,
            None => sample.len() < target,
        };
        if let Some(delimiter) = detect_delimiter(&sample, at_eof, encoding) {
            return Ok(delimiter);
        }
        if sample.len() >= limit {
            return delimiter_at_limit(file, file_size, at_eof, &sample, limit, encoding);
        }
        if sample.len() < target {
            return Err(QuarryError::SourceChanged);
        }
        // A partial first record is not evidence for a one-column comma file.
        // Grow only until a complete record is available, within the caller's cap.
        let next = target.saturating_mul(2).min(limit);
        target = if encoding.is_utf16() { next & !1 } else { next };
        if target <= sample.len() {
            return delimiter_at_limit(file, file_size, at_eof, &sample, limit, encoding);
        }
    }
}

/// A stream sample that filled the bootstrap limit may still be the whole stream:
/// probe one byte past it and, at the end, detect from the complete sample.
fn delimiter_at_limit(
    file: &mut impl Read,
    file_size: Option<u64>,
    at_eof: bool,
    sample: &[u8],
    limit: usize,
    encoding: Encoding,
) -> Result<u8, QuarryError> {
    if file_size.is_none()
        && !at_eof
        && stream_ends_here(file)?
        && let Some(delimiter) = detect_delimiter(sample, true, encoding)
    {
        return Ok(delimiter);
    }
    Err(QuarryError::BootstrapLimitExceeded {
        limit,
        rows_found: 0,
    })
}

/// Read one byte past a bounded prefix to learn whether a stream ends there.
/// The byte is discarded; callers either stop or seek back.
fn stream_ends_here(file: &mut impl Read) -> io::Result<bool> {
    let mut probe = [0_u8; 1];
    loop {
        match file.read(&mut probe) {
            Ok(read) => return Ok(read == 0),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
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

fn read_bootstrap_rows_from_open_file(
    file: &File,
    delimiter: u8,
    encoding: Encoding,
    end: u64,
    max_rows: usize,
    byte_limit: usize,
) -> Result<Vec<Row>, QuarryError> {
    if end == 0 || max_rows == 0 || byte_limit == 0 {
        return Ok(Vec::new());
    }
    let mut limit = usize::try_from(end).unwrap_or(byte_limit).min(byte_limit);
    if encoding.is_utf16() {
        limit &= !1;
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut scanner = RecordScanner::new(delimiter, encoding)?;
    let mut bytes = Vec::with_capacity(DEFAULT_READ_CHUNK.min(limit));
    let mut chunk = vec![0; encoding.chunk_size(DEFAULT_READ_CHUNK.min(limit))];
    let mut ends = Vec::with_capacity(max_rows);
    let mut offset = 0u64;

    while ends.len() < max_rows && bytes.len() < limit {
        let mut allowed = (limit - bytes.len()).min(chunk.len());
        if encoding.is_utf16() {
            allowed &= !1;
        }
        if allowed == 0 {
            break;
        }
        let read = positional_io::read_at_exact_or_eof(file, offset, &mut chunk[..allowed])?;
        if read < allowed {
            return Err(QuarryError::SourceChanged);
        }
        bytes.extend_from_slice(&chunk[..read]);
        scanner.scan_chunk(&chunk[..read], offset, |rec_end| {
            if ends.len() < max_rows {
                ends.push(rec_end);
            }
        })?;
        offset += read as u64;
    }

    if offset == end && ends.len() < max_rows {
        scanner.finish(offset, |rec_end| {
            if ends.len() < max_rows {
                ends.push(rec_end);
            }
        })?;
    }
    let mut rows = Vec::with_capacity(ends.len());
    let mut start = 0;
    for (physical_row, &rec_end) in ends.iter().enumerate() {
        let rec_end = rec_end as usize;
        match parse_encoded_record_with_bom_and_field_limit(
            &bytes[start..rec_end],
            delimiter,
            physical_row as u64,
            physical_row == 0,
            encoding,
            MAX_TRANSFORMATION_COLUMNS,
        ) {
            Ok(fields) => rows.push(Row {
                offset: start as u64,
                fields: fields.into_iter().map(|field| field.into_owned()).collect(),
            }),
            Err(QuarryError::Parse(err))
                if matches!(
                    err.kind,
                    quarry_delimited::ParseErrorKind::FieldLimitExceeded(_)
                ) =>
            {
                // Keep any already-parsed bootstrap preview rows and stop before materializing
                // an overly wide row during watch stop reconciliation.
                break;
            }
            Err(err) => return Err(err),
        }
        start = rec_end;
    }
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn read_rows_from_open_file(
    file: &File,
    delimiter: u8,
    index: &StructuralIndex,
    start: u64,
    count: usize,
    max_record_bytes: usize,
    encoding: Encoding,
    end: u64,
    strip_bom_at_zero: bool,
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
    let mut scanner = RecordScanner::at_offset_with_bom_handling(
        delimiter,
        checkpoint.offset,
        encoding,
        strip_bom_at_zero,
    )?;
    let mut chunk = vec![0; encoding.chunk_size(DEFAULT_READ_CHUNK)];
    let mut absolute_start = checkpoint.offset;
    let mut row_number = checkpoint.row;
    let mut record_start = checkpoint.offset;
    let mut record = Vec::new();
    let mut rows = Vec::with_capacity((target_end - start) as usize);

    while row_number < target_end {
        let remaining = end.saturating_sub(absolute_start);
        let read = if remaining == 0 {
            0
        } else {
            if encoding.is_utf16() && remaining == 1 {
                return Err(QuarryError::SourceChanged);
            }
            let mut limit = chunk
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            if encoding.is_utf16() {
                limit &= !1;
            }
            if limit == 0 {
                return Err(QuarryError::SourceChanged);
            }
            let got =
                positional_io::read_at_exact_or_eof(file, absolute_start, &mut chunk[..limit])?;
            if got < limit {
                // Any short read below the committed frontier indicates concurrent truncation.
                return Err(QuarryError::SourceChanged);
            }
            got
        };
        if read == 0 {
            if !record.is_empty() && row_number >= start && row_number < target_end {
                let fields = parse_encoded_record_with_bom_and_field_limit(
                    &record,
                    delimiter,
                    row_number,
                    strip_bom_at_zero && record_start == 0,
                    encoding,
                    MAX_TRANSFORMATION_COLUMNS,
                )?
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
                    match parse_encoded_record_with_bom_and_field_limit(
                        &record,
                        delimiter,
                        row_number,
                        strip_bom_at_zero && record_start == 0,
                        encoding,
                        MAX_TRANSFORMATION_COLUMNS,
                    ) {
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

#[allow(clippy::too_many_arguments)]
fn read_rows_from_index(
    path: &Path,
    delimiter: u8,
    index: &StructuralIndex,
    start: u64,
    count: usize,
    max_record_bytes: usize,
    encoding: Encoding,
    // Bytes at or past this offset are never read, as when a live index has not
    // reached them yet.
    end: u64,
    strip_bom_at_zero: bool,
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
    if encoding.is_utf16() {
        file.seek(SeekFrom::Start(checkpoint.offset))?;
    }
    let mut scanner = RecordScanner::at_offset_with_bom_handling(
        delimiter,
        checkpoint.offset,
        encoding,
        strip_bom_at_zero,
    )?;
    let mut chunk = vec![0; encoding.chunk_size(DEFAULT_READ_CHUNK)];
    let mut absolute_start = checkpoint.offset;
    let mut row_number = checkpoint.row;
    let mut record_start = checkpoint.offset;
    let mut record = Vec::new();
    let mut rows = Vec::with_capacity((target_end - start) as usize);

    while row_number < target_end {
        // Bound the read itself, so bytes past `end` (such as half of a UTF-16
        // code unit still being written, or bytes beyond a section's end) are never read.
        let remaining = end.saturating_sub(absolute_start);
        let read = if remaining == 0 {
            0
        } else {
            let limit = chunk
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            if encoding.is_utf16() {
                encoding.read_chunk(&mut file, &mut chunk[..limit])?
            } else {
                positional_io::read_at_exact_or_eof(&file, absolute_start, &mut chunk[..limit])?
            }
        };
        if read == 0 {
            if !record.is_empty() && row_number >= start && row_number < target_end {
                let fields = parse_encoded_record_with_bom_and_field_limit(
                    &record,
                    delimiter,
                    row_number,
                    strip_bom_at_zero && record_start == 0,
                    encoding,
                    MAX_TRANSFORMATION_COLUMNS,
                )?
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
                    match parse_encoded_record_with_bom_and_field_limit(
                        &record,
                        delimiter,
                        row_number,
                        strip_bom_at_zero && record_start == 0,
                        encoding,
                        MAX_TRANSFORMATION_COLUMNS,
                    ) {
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

    use super::{Encoding, HeaderMode, IndexConfig, OpenOptions, QuarryError, Session};

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture(bytes: &[u8]) -> std::path::PathBuf {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("quarry-{}-{id}.csv", std::process::id()));
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    #[test]
    fn live_utf16_reads_never_read_past_the_snapshot_boundary() {
        let mut bytes = vec![0xff, 0xfe];
        for unit in "id,name\n1,a\n2,b\n".encode_utf16() {
            bytes.extend(unit.to_le_bytes());
        }
        let path = fixture(&bytes);
        let session =
            Session::open_with_encoding(&path, OpenOptions::default(), Some(Encoding::Utf16Le))
                .unwrap();
        let index = session
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        // A writer has written half of the next code unit past the snapshot.
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"3")
            .unwrap();
        let rows = super::read_rows_from_index(
            &path,
            b',',
            &index,
            0,
            10,
            super::DEFAULT_MAX_RECORD_BYTES,
            Encoding::Utf16Le,
            bytes.len() as u64,
            true,
        )
        .unwrap();
        let fields: Vec<_> = rows.into_iter().map(|row| row.fields).collect();
        assert_eq!(
            fields,
            [["id", "name"], ["1", "a"], ["2", "b"]]
                .map(|row| row.map(|field| field.as_bytes().to_vec()).to_vec())
        );
        fs::remove_file(path).unwrap();
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

    #[test]
    fn record_start_offsets_batches_checkpoints_honors_cancel_and_source_guard() {
        let mut bytes = Vec::from(&b"id,val\n"[..]);
        for i in 1..=64_u64 {
            bytes.extend_from_slice(format!("{i},row_value_{i:03}\n").as_bytes());
        }
        let path = fixture(&bytes);
        let session = Session::open(
            &path,
            OpenOptions {
                rows: 4,
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let index = session
            .start_indexing(IndexConfig {
                checkpoint_every: 8,
                ..IndexConfig::default()
            })
            .unwrap()
            .wait()
            .unwrap();

        let cancel = std::sync::atomic::AtomicBool::new(false);
        let query_rows = [55, 1, 24, 10, 24, 64, 3, 17];
        let resolved = session
            .record_start_offsets(&index, &query_rows, &cancel)
            .unwrap()
            .expect("uncancelled offset resolution should return offsets");
        let mut expected_rows = query_rows.to_vec();
        expected_rows.sort_unstable();
        expected_rows.dedup();
        let expected: Vec<(u64, u64)> = expected_rows
            .into_iter()
            .map(|row| {
                let loaded = session.read_rows(&index, row, 1).unwrap();
                (row, loaded[0].offset)
            })
            .collect();
        assert_eq!(resolved, expected);

        let cancelled = std::sync::atomic::AtomicBool::new(true);
        assert_eq!(
            session
                .record_start_offsets(&index, &[20, 40], &cancelled)
                .unwrap(),
            None
        );

        fs::write(&path, b"id,val\n999,changed_externally\n").unwrap();
        assert!(matches!(
            session.record_start_offsets(&index, &[20], &cancel),
            Err(QuarryError::SourceChanged)
        ));
        fs::remove_file(path).unwrap();
    }
}

mod working_storage;
pub use working_storage::{WorkingCopyCleanup, WorkingCopyStorage};
