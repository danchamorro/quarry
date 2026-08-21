mod export;
mod filter;
mod index;
mod search;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub use export::{
    ColumnTransformation, FilterExportJob, FilterExportOutcome, FilterExportProgress,
    FilterExportSummary, MAX_TRANSFORMATION_COLUMNS, SaveAsJob, SaveAsOutcome, SaveAsProgress,
    SaveAsSummary, SplitAnalysisJob, SplitAnalysisOutcome, SplitAnalysisProgress,
    SplitAnalysisSummary,
};
pub use filter::{
    FilterIndex, FilterJob, FilterMatch, FilterOperator, FilterPredicate, FilterProgress,
    FilterQuery, FilterReadJob, FilterReadOutcome, FilterReadProgress,
};
pub use index::{Checkpoint, IndexConfig, IndexJob, IndexProgress, StructuralIndex};
use quarry_delimited::{ParseError, RecordScanner, parse_record};
pub use search::{SearchJob, SearchMatch, SearchOutcome, SearchPosition, SearchProgress};

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

#[derive(Debug)]
pub enum QuarryError {
    Io(io::Error),
    Parse(ParseError),
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
    InvalidOption(&'static str),
    ExportDestinationIsSource,
    ExportDestinationExists,
    SourceChanged,
    WorkerPanicked,
}

impl fmt::Display for QuarryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Parse(error) => error.fmt(f),
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
            Self::Parse(error) => Some(error),
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

#[derive(Debug, Clone, Copy)]
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
    pub file_size: u64,
    pub dialect: Dialect,
    pub first_rows: Vec<Row>,
    pub metrics: OpenMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceStamp {
    len: u64,
    modified: Option<SystemTime>,
    readonly: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanos: i64,
}

impl SourceStamp {
    pub(crate) fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            readonly: metadata.permissions().readonly(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanos: metadata.ctime_nsec(),
        }
    }
}

impl Session {
    pub fn open(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self, QuarryError> {
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
        let metadata = file.metadata()?;
        let file_size = metadata.len();
        let source_stamp = SourceStamp::from_metadata(&metadata);

        let mut sample = vec![0; options.sample_bytes.min(file_size as usize)];
        let sample_len = read_up_to(&mut file, &mut sample)?;
        sample.truncate(sample_len);
        let delimiter = match options.delimiter {
            Some(delimiter) => {
                RecordScanner::new(delimiter)?;
                delimiter
            }
            None => detect_delimiter(&sample),
        };
        drop(sample);

        file.seek(SeekFrom::Start(0))?;
        let (bootstrap, ends) = read_initial_records(
            &mut file,
            file_size,
            delimiter,
            options.rows,
            options.bootstrap_limit,
        )?;
        let first_rows =
            materialize_rows(&bootstrap, &ends[..ends.len().min(options.rows)], delimiter)?;
        let has_header = match options.header_mode {
            HeaderMode::Auto => detect_header(&first_rows),
            HeaderMode::FirstRow => true,
            HeaderMode::NoHeader => false,
        };

        Ok(Self {
            path: path.as_ref().to_path_buf(),
            source_stamp,
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

    pub fn start_indexing(&self, config: IndexConfig) -> Result<IndexJob, QuarryError> {
        IndexJob::start(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            config,
        )
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
        )
    }
}

fn read_initial_records(
    file: &mut File,
    file_size: u64,
    delimiter: u8,
    wanted: usize,
    limit: usize,
) -> Result<(Vec<u8>, Vec<u64>), QuarryError> {
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut bytes = Vec::with_capacity(DEFAULT_READ_CHUNK.min(limit));
    let mut chunk = vec![0; DEFAULT_READ_CHUNK.min(limit)];
    let mut ends = Vec::with_capacity(wanted);

    while ends.len() < wanted && bytes.len() < limit {
        let allowed = (limit - bytes.len()).min(chunk.len());
        let read = file.read(&mut chunk[..allowed])?;
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

fn materialize_rows(bytes: &[u8], ends: &[u64], delimiter: u8) -> Result<Vec<Row>, QuarryError> {
    let mut rows = Vec::with_capacity(ends.len());
    let mut start = 0;
    for (physical_row, &end) in ends.iter().enumerate() {
        let end = end as usize;
        let fields = parse_source_record(&bytes[start..end], delimiter, physical_row as u64)?
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

fn detect_delimiter(sample: &[u8]) -> u8 {
    let mut best = (0_usize, 0_usize, b',');
    for delimiter in *b",\t|;" {
        let Ok(mut scanner) = RecordScanner::new(delimiter) else {
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

        let mut start = 0;
        let mut frequencies = HashMap::new();
        for (physical_row, end) in ends.into_iter().enumerate() {
            let end = end as usize;
            let Ok(fields) =
                parse_source_record(&sample[start..end], delimiter, physical_row as u64)
            else {
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
    best.2
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
    let mut scanner = RecordScanner::at_offset(delimiter, checkpoint.offset)?;
    let mut chunk = vec![0; DEFAULT_READ_CHUNK];
    let mut absolute_start = checkpoint.offset;
    let mut row_number = checkpoint.row;
    let mut record_start = checkpoint.offset;
    let mut record = Vec::new();
    let mut rows = Vec::with_capacity(count);

    while row_number < target_end {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            if !record.is_empty() && row_number >= start && row_number < target_end {
                let fields = parse_source_record(&record, delimiter, row_number)?
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
                    match parse_source_record(&record, delimiter, row_number) {
                        Ok(fields) => rows.push(Row {
                            offset: record_start,
                            fields: fields.into_iter().map(|field| field.into_owned()).collect(),
                        }),
                        Err(error) => deferred_error = Some(error.into()),
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

fn read_up_to(reader: &mut impl Read, bytes: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    while read < bytes.len() {
        let count = reader.read(&mut bytes[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    Ok(read)
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
}
