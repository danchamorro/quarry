use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::RangeInclusive;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use memchr::{memchr, memmem::Finder};
use quarry_delimited::{
    ParseErrorKind, RecordScanner, parse_record, parse_record_with_field_limit,
};

use crate::case::ByteMatcher;
use crate::filter::{FilterQuery, matching_fields, predicate_groups, validate_query};
use crate::{
    CaseSensitivity, DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, QuarryError, Session,
    SourceStamp,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct ExportConfig {
    chunk_bytes: usize,
    max_record_bytes: usize,
}

const DEFAULT_EXPORT_CONFIG: ExportConfig = ExportConfig {
    chunk_bytes: DEFAULT_READ_CHUNK,
    max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
};
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterExportSummary {
    pub destination: PathBuf,
    pub rows_written: u64,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterExportOutcome {
    Complete(FilterExportSummary),
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
pub struct FilterExportProgress {
    pub bytes_scanned: u64,
    pub rows_scanned: u64,
    pub rows_written: u64,
    pub bytes_written: u64,
    pub total_bytes: u64,
    pub elapsed: Duration,
    pub done: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveAsSummary {
    pub destination: PathBuf,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveAsOutcome {
    Complete(SaveAsSummary),
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
pub struct SaveAsProgress {
    pub bytes_scanned: u64,
    pub bytes_written: u64,
    pub total_bytes: u64,
    pub elapsed: Duration,
    pub done: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralReplacement {
    pub needle: Vec<u8>,
    pub replacement: Vec<u8>,
    pub case_sensitivity: CaseSensitivity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceAllSummary {
    pub destination: PathBuf,
    pub bytes_written: u64,
    pub replacements: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceAllOutcome {
    Complete(ReplaceAllSummary),
    NoMatch,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitAnalysisSummary {
    pub rows_scanned: u64,
    pub max_pieces: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitAnalysisOutcome {
    Complete(SplitAnalysisSummary),
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
pub struct SplitAnalysisProgress {
    pub bytes_scanned: u64,
    pub rows_scanned: u64,
    pub total_bytes: u64,
    pub elapsed: Duration,
    pub done: bool,
    pub cancelled: bool,
}

pub const MAX_TRANSFORMATION_COLUMNS: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnTransformation {
    Split {
        source_column: usize,
        separator: Vec<u8>,
        output_count: usize,
        output_headers: Option<Vec<Vec<u8>>>,
    },
    Join {
        source_columns: Vec<usize>,
        separator: Vec<u8>,
        output_header: Option<Vec<u8>>,
    },
    Arrange {
        source_width: usize,
        output_columns: Vec<usize>,
    },
}

impl ColumnTransformation {
    pub fn split_with_blank_headers(
        source_column: usize,
        separator: Vec<u8>,
        output_count: usize,
        source_header: Option<Vec<u8>>,
    ) -> Result<Self, QuarryError> {
        let has_header = source_header.is_some();
        let output_headers = source_header.map(|header| {
            let mut headers = vec![Vec::new(); output_count];
            if let Some(first) = headers.first_mut() {
                *first = header;
            }
            headers
        });
        let transformation = Self::Split {
            source_column,
            separator,
            output_count,
            output_headers,
        };
        transformation.validate_for_header(has_header)?;
        Ok(transformation)
    }

    pub fn transform_fields(&self, fields: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, QuarryError> {
        self.validate_shape()?;
        self.validate_input_width(fields.len())?;
        let joined_columns = self.joined_columns();
        self.transform_fields_unchecked(
            fields.to_vec(),
            &joined_columns,
            crate::DEFAULT_MAX_RECORD_BYTES,
        )
    }

    pub fn transform_header_fields(&self, fields: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, QuarryError> {
        self.validate_for_header(true)?;
        self.validate_input_width(fields.len())?;
        let joined_columns = self.joined_columns();
        self.transform_header_fields_unchecked(fields.to_vec(), &joined_columns)
    }

    fn transform_fields_unchecked(
        &self,
        mut fields: Vec<Vec<u8>>,
        joined_columns: &BTreeSet<usize>,
        max_record_bytes: usize,
    ) -> Result<Vec<Vec<u8>>, QuarryError> {
        self.validate_input_width(fields.len())?;
        match self {
            Self::Split {
                source_column,
                separator,
                output_count,
                ..
            } => {
                fields.resize_with(source_column.saturating_add(1).max(fields.len()), Vec::new);
                let parts = split_field(&fields[*source_column], separator, *output_count);
                fields.splice(*source_column..=*source_column, parts);
                Ok(fields)
            }
            Self::Join {
                source_columns,
                separator,
                ..
            } => {
                let last_column = *source_columns
                    .iter()
                    .max()
                    .expect("join columns are validated");
                fields.resize_with(last_column.saturating_add(1).max(fields.len()), Vec::new);

                let joined_len = source_columns.iter().fold(0_usize, |length, column| {
                    length.saturating_add(fields[*column].len())
                });
                let joined_len = joined_len
                    .saturating_add(separator.len().saturating_mul(source_columns.len() - 1));
                if joined_len > max_record_bytes {
                    return Err(QuarryError::RecordTooLarge {
                        limit: max_record_bytes,
                    });
                }
                let mut joined = Vec::with_capacity(joined_len);
                for (index, column) in source_columns.iter().enumerate() {
                    if index > 0 {
                        joined.extend_from_slice(separator);
                    }
                    joined.extend_from_slice(&fields[*column]);
                }

                let insertion = *source_columns
                    .iter()
                    .min()
                    .expect("join columns are validated");
                let mut joined = Some(joined);
                let mut output = Vec::with_capacity(fields.len() - source_columns.len() + 1);
                for (column, field) in fields.into_iter().enumerate() {
                    if column == insertion {
                        output.push(joined.take().expect("joined field is inserted once"));
                    }
                    if !joined_columns.contains(&column) {
                        output.push(field);
                    }
                }
                Ok(output)
            }
            Self::Arrange {
                source_width,
                output_columns,
            } => Ok(arrange_fields(fields, *source_width, output_columns)),
        }
    }

    fn validate_for_header(&self, has_header: bool) -> Result<(), QuarryError> {
        self.validate_shape()?;
        match self {
            Self::Split {
                output_count,
                output_headers,
                ..
            } => match (has_header, output_headers) {
                (true, Some(headers)) if headers.len() == *output_count => Ok(()),
                (true, Some(_)) => Err(QuarryError::InvalidOption(
                    "split output header count must match output count",
                )),
                (true, None) => Err(QuarryError::InvalidOption(
                    "split output headers are required for a headered source",
                )),
                (false, Some(_)) => Err(QuarryError::InvalidOption(
                    "split output headers require a headered source",
                )),
                (false, None) => Ok(()),
            },
            Self::Join { output_header, .. } => match (has_header, output_header) {
                (true, Some(_)) | (false, None) => Ok(()),
                (true, None) => Err(QuarryError::InvalidOption(
                    "join output header is required for a headered source",
                )),
                (false, Some(_)) => Err(QuarryError::InvalidOption(
                    "join output header requires a headered source",
                )),
            },
            Self::Arrange { .. } => Ok(()),
        }
    }

    fn validate_shape(&self) -> Result<(), QuarryError> {
        match self {
            Self::Split {
                source_column,
                separator,
                output_count,
                ..
            } => {
                if separator.is_empty() {
                    return Err(QuarryError::InvalidOption(
                        "split separator must not be empty",
                    ));
                }
                if *output_count < 2 {
                    return Err(QuarryError::InvalidOption(
                        "split output count must be at least two",
                    ));
                }
                if *source_column >= MAX_TRANSFORMATION_COLUMNS {
                    return Err(QuarryError::InvalidOption(
                        "split source column exceeds the supported limit",
                    ));
                }
                if *output_count > MAX_TRANSFORMATION_COLUMNS {
                    return Err(QuarryError::InvalidOption(
                        "split output count exceeds the supported limit",
                    ));
                }
            }
            Self::Join { source_columns, .. } => {
                if source_columns.len() < 2 {
                    return Err(QuarryError::InvalidOption(
                        "join requires at least two source columns",
                    ));
                }
                if source_columns.len() > MAX_TRANSFORMATION_COLUMNS
                    || source_columns
                        .iter()
                        .any(|column| *column >= MAX_TRANSFORMATION_COLUMNS)
                {
                    return Err(QuarryError::InvalidOption(
                        "join source columns exceed the supported limit",
                    ));
                }
                if source_columns
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .len()
                    != source_columns.len()
                {
                    return Err(QuarryError::InvalidOption(
                        "join source columns must be unique",
                    ));
                }
            }
            Self::Arrange {
                source_width,
                output_columns,
            } => {
                if *source_width == 0 || *source_width > MAX_TRANSFORMATION_COLUMNS {
                    return Err(QuarryError::InvalidOption(
                        "arrange source width exceeds the supported limit",
                    ));
                }
                if output_columns.is_empty() {
                    return Err(QuarryError::InvalidOption(
                        "arrange requires at least one output column",
                    ));
                }
                if output_columns.len() > MAX_TRANSFORMATION_COLUMNS
                    || output_columns.iter().any(|column| *column >= *source_width)
                {
                    return Err(QuarryError::InvalidOption(
                        "arrange output columns exceed the supported source width",
                    ));
                }
                if output_columns
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .len()
                    != output_columns.len()
                {
                    return Err(QuarryError::InvalidOption(
                        "arrange output columns must be unique",
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_input_width(&self, input_columns: usize) -> Result<(), QuarryError> {
        if input_columns > MAX_TRANSFORMATION_COLUMNS {
            return Err(QuarryError::InvalidOption(
                "source record column count exceeds the supported limit",
            ));
        }
        if let Self::Split {
            source_column,
            output_count,
            ..
        } = self
        {
            let padded_columns = input_columns.max(source_column.saturating_add(1));
            let output_columns = padded_columns
                .saturating_sub(1)
                .saturating_add(*output_count);
            if output_columns > MAX_TRANSFORMATION_COLUMNS {
                return Err(QuarryError::InvalidOption(
                    "split output column count exceeds the supported limit",
                ));
            }
        }
        Ok(())
    }

    fn replaces_source_column(&self, column: usize) -> bool {
        match self {
            Self::Split { source_column, .. } => column == *source_column,
            Self::Join { source_columns, .. } => source_columns.contains(&column),
            Self::Arrange {
                source_width,
                output_columns,
            } => column < *source_width && !output_columns.contains(&column),
        }
    }

    fn joined_columns(&self) -> BTreeSet<usize> {
        match self {
            Self::Split { .. } | Self::Arrange { .. } => BTreeSet::new(),
            Self::Join { source_columns, .. } => source_columns.iter().copied().collect(),
        }
    }

    fn transform_header_fields_unchecked(
        &self,
        mut fields: Vec<Vec<u8>>,
        joined_columns: &BTreeSet<usize>,
    ) -> Result<Vec<Vec<u8>>, QuarryError> {
        self.validate_input_width(fields.len())?;
        if let Self::Arrange {
            source_width,
            output_columns,
        } = self
        {
            return Ok(arrange_fields(fields, *source_width, output_columns));
        }
        let last_source_column = match self {
            Self::Split { source_column, .. } => *source_column,
            Self::Join { source_columns, .. } => *source_columns
                .iter()
                .max()
                .expect("join columns are validated"),
            Self::Arrange { .. } => unreachable!("arrange returned above"),
        };
        fields.resize_with(
            fields.len().max(last_source_column.saturating_add(1)),
            Vec::new,
        );

        match self {
            Self::Split {
                source_column,
                output_headers: Some(headers),
                ..
            } => {
                let mut transformed = Vec::with_capacity(fields.len() - 1 + headers.len());
                for (column, field) in fields.into_iter().enumerate() {
                    if column == *source_column {
                        transformed.extend(headers.iter().cloned());
                    } else {
                        transformed.push(field);
                    }
                }
                Ok(transformed)
            }
            Self::Join {
                source_columns,
                output_header: Some(header),
                ..
            } => {
                let insertion = *source_columns
                    .iter()
                    .min()
                    .expect("join columns are validated");
                let mut transformed = Vec::with_capacity(fields.len() - source_columns.len() + 1);
                for (column, field) in fields.into_iter().enumerate() {
                    if column == insertion {
                        transformed.push(header.clone());
                    }
                    if !joined_columns.contains(&column) {
                        transformed.push(field);
                    }
                }
                Ok(transformed)
            }
            _ => unreachable!("header options are validated before saving"),
        }
    }
}

fn arrange_fields(
    mut fields: Vec<Vec<u8>>,
    source_width: usize,
    output_columns: &[usize],
) -> Vec<Vec<u8>> {
    let trailing = if fields.len() > source_width {
        fields.split_off(source_width)
    } else {
        Vec::new()
    };
    let mut output = Vec::with_capacity(output_columns.len() + trailing.len());
    output.extend(output_columns.iter().map(|column| {
        fields
            .get_mut(*column)
            .map(std::mem::take)
            .unwrap_or_default()
    }));
    output.extend(trailing);
    output
}

struct PreparedColumnTransformation {
    transformation: ColumnTransformation,
    joined_columns: BTreeSet<usize>,
}

impl PreparedColumnTransformation {
    fn new(transformation: ColumnTransformation, has_header: bool) -> Result<Self, QuarryError> {
        transformation.validate_for_header(has_header)?;
        let joined_columns = transformation.joined_columns();
        Ok(Self {
            transformation,
            joined_columns,
        })
    }

    fn transform_fields(
        &self,
        fields: Vec<Vec<u8>>,
        max_record_bytes: usize,
    ) -> Result<Vec<Vec<u8>>, QuarryError> {
        self.transformation.transform_fields_unchecked(
            fields,
            &self.joined_columns,
            max_record_bytes,
        )
    }

    fn transform_header_fields(&self, fields: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, QuarryError> {
        self.transformation
            .transform_header_fields_unchecked(fields, &self.joined_columns)
    }
}

fn split_field(field: &[u8], separator: &[u8], output_count: usize) -> Vec<Vec<u8>> {
    let mut parts = Vec::with_capacity(output_count);
    if separator.len() > field.len() {
        parts.push(field.to_vec());
        parts.resize_with(output_count, Vec::new);
        return parts;
    }

    let finder = Finder::new(separator);
    let mut remainder = field;
    for _ in 1..output_count {
        let Some(position) = finder.find(remainder) else {
            parts.push(remainder.to_vec());
            remainder = &[];
            while parts.len() + 1 < output_count {
                parts.push(Vec::new());
            }
            break;
        };
        parts.push(remainder[..position].to_vec());
        remainder = &remainder[position + separator.len()..];
    }
    parts.push(remainder.to_vec());
    parts
}

struct SharedState {
    bytes_scanned: AtomicU64,
    rows_scanned: AtomicU64,
    rows_written: AtomicU64,
    bytes_written: AtomicU64,
    finished_nanos: AtomicU64,
    done: AtomicBool,
    cancel_requested: AtomicBool,
    cancelled: AtomicBool,
    error: Mutex<Option<String>>,
    started: Instant,
    total_bytes: u64,
}

impl SharedState {
    fn new(total_bytes: u64) -> Self {
        Self {
            bytes_scanned: AtomicU64::new(0),
            rows_scanned: AtomicU64::new(0),
            rows_written: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            finished_nanos: AtomicU64::new(0),
            done: AtomicBool::new(false),
            cancel_requested: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            error: Mutex::new(None),
            started: Instant::now(),
            total_bytes,
        }
    }
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

pub struct FilterExportJob {
    shared: Arc<SharedState>,
    handle: Option<JoinHandle<Result<FilterExportOutcome, QuarryError>>>,
}

impl FilterExportJob {
    fn start(
        source_path: PathBuf,
        file_size: u64,
        delimiter: u8,
        has_header: bool,
        query: FilterQuery,
        destination: PathBuf,
        config: ExportConfig,
    ) -> Result<Self, QuarryError> {
        if config.chunk_bytes == 0 {
            return Err(QuarryError::InvalidOption(
                "filter export chunk must be non-zero",
            ));
        }
        validate_query(&query)?;
        let source = File::open(&source_path)?;
        let output = ExportTarget::new(&source_path, destination)?;
        let shared = Arc::new(SharedState::new(file_size));
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-export".into())
            .spawn(move || {
                let _completion = WorkerCompletion(&worker_state);
                let result = run_export(
                    source,
                    output,
                    delimiter,
                    u64::from(has_header),
                    &query,
                    config,
                    &worker_state,
                );
                match &result {
                    Ok(FilterExportOutcome::Cancelled) => {
                        worker_state.cancelled.store(true, Ordering::Release);
                    }
                    Err(error) => {
                        *worker_state.error.lock().unwrap() = Some(error.to_string());
                    }
                    Ok(FilterExportOutcome::Complete(_)) => {}
                }
                result
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> FilterExportProgress {
        let done = self.shared.done.load(Ordering::Acquire);
        let finished_nanos = self.shared.finished_nanos.load(Ordering::Acquire);
        FilterExportProgress {
            bytes_scanned: self.shared.bytes_scanned.load(Ordering::Acquire),
            rows_scanned: self.shared.rows_scanned.load(Ordering::Acquire),
            rows_written: self.shared.rows_written.load(Ordering::Acquire),
            bytes_written: self.shared.bytes_written.load(Ordering::Acquire),
            total_bytes: self.shared.total_bytes,
            elapsed: if finished_nanos == 0 {
                self.shared.started.elapsed()
            } else {
                Duration::from_nanos(finished_nanos)
            },
            done,
            cancelled: self.shared.cancelled.load(Ordering::Acquire),
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        if !self.shared.done.load(Ordering::Acquire) {
            self.shared.cancel_requested.store(true, Ordering::Release);
        }
    }

    pub fn wait(mut self) -> Result<FilterExportOutcome, QuarryError> {
        self.handle
            .take()
            .expect("filter export handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl Drop for FilterExportJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

pub struct SplitAnalysisJob {
    shared: Arc<SharedState>,
    handle: Option<JoinHandle<Result<SplitAnalysisOutcome, QuarryError>>>,
}

impl SplitAnalysisJob {
    #[allow(clippy::too_many_arguments)]
    fn start(
        source_path: PathBuf,
        file_size: u64,
        delimiter: u8,
        has_header: bool,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        source_column: usize,
        separator: Vec<u8>,
        max_pieces: usize,
        source_stamp: SourceStamp,
        config: ExportConfig,
    ) -> Result<Self, QuarryError> {
        if config.chunk_bytes == 0 {
            return Err(QuarryError::InvalidOption(
                "split analysis chunk must be non-zero",
            ));
        }
        if separator.is_empty() {
            return Err(QuarryError::InvalidOption(
                "split separator must not be empty",
            ));
        }
        if source_column >= MAX_TRANSFORMATION_COLUMNS {
            return Err(QuarryError::InvalidOption(
                "split source column exceeds the supported limit",
            ));
        }
        if max_pieces == 0 || max_pieces > MAX_TRANSFORMATION_COLUMNS {
            return Err(QuarryError::InvalidOption(
                "split analysis maximum pieces must be within the supported limit",
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

        let source = File::open(&source_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
        if !source_matches_stamp(&source, &source_path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }

        let shared = Arc::new(SharedState::new(file_size));
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-split-analysis".into())
            .spawn(move || {
                let _completion = WorkerCompletion(&worker_state);
                let result = run_split_analysis(
                    source,
                    &source_path,
                    &source_stamp,
                    delimiter,
                    has_header,
                    &cell_edits,
                    source_column,
                    &separator,
                    max_pieces,
                    config,
                    &worker_state,
                );
                match &result {
                    Ok(SplitAnalysisOutcome::Cancelled) => {
                        worker_state.cancelled.store(true, Ordering::Release);
                    }
                    Err(error) => {
                        *worker_state.error.lock().unwrap() = Some(error.to_string());
                    }
                    Ok(SplitAnalysisOutcome::Complete(_)) => {}
                }
                result
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> SplitAnalysisProgress {
        let done = self.shared.done.load(Ordering::Acquire);
        let finished_nanos = self.shared.finished_nanos.load(Ordering::Acquire);
        SplitAnalysisProgress {
            bytes_scanned: self.shared.bytes_scanned.load(Ordering::Acquire),
            rows_scanned: self.shared.rows_scanned.load(Ordering::Acquire),
            total_bytes: self.shared.total_bytes,
            elapsed: if finished_nanos == 0 {
                self.shared.started.elapsed()
            } else {
                Duration::from_nanos(finished_nanos)
            },
            done,
            cancelled: self.shared.cancelled.load(Ordering::Acquire),
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        if !self.shared.done.load(Ordering::Acquire) {
            self.shared.cancel_requested.store(true, Ordering::Release);
        }
    }

    pub fn wait(mut self) -> Result<SplitAnalysisOutcome, QuarryError> {
        self.handle
            .take()
            .expect("split analysis handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl Drop for SplitAnalysisJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

enum SaveWorkerOutcome {
    Complete {
        summary: SaveAsSummary,
        replacements: u64,
    },
    NoMatch,
    Cancelled,
}

pub struct SaveAsJob {
    shared: Arc<SharedState>,
    handle: Option<JoinHandle<Result<SaveWorkerOutcome, QuarryError>>>,
}

pub struct ReplaceAllJob {
    inner: SaveAsJob,
}

enum SaveTarget {
    New(PathBuf, SourceStamp),
    WorkingCopy(PathBuf, SourceStamp),
    Source(SourceStamp),
    Existing {
        destination: PathBuf,
        source_expected: SourceStamp,
        destination_expected: SourceStamp,
    },
}

struct SaveEdits {
    headers: BTreeMap<usize, Vec<u8>>,
    cells: BTreeMap<(u64, usize), Vec<u8>>,
    transformation: Option<ColumnTransformation>,
}

struct PreparedSaveEdits {
    headers: BTreeMap<usize, Vec<u8>>,
    cells: BTreeMap<(u64, usize), Vec<u8>>,
    transformation: Option<PreparedColumnTransformation>,
    replacement: Option<LiteralReplacement>,
    deleted_rows: Vec<RangeInclusive<u64>>,
}

impl PreparedSaveEdits {
    fn validate_size_lower_bounds(&self, max_record_bytes: usize) -> Result<(), QuarryError> {
        let record_too_large = || QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        };
        let mut header_clone_bytes = self
            .headers
            .iter()
            .filter(|(column, _)| {
                self.transformation.as_ref().is_none_or(|transformation| {
                    !transformation
                        .transformation
                        .replaces_source_column(**column)
                })
            })
            .fold(0_usize, |length, (_, value)| {
                length.saturating_add(value.len())
            });
        let mut current_row = None;
        let mut row_value_bytes = 0_usize;
        for ((row, column), value) in &self.cells {
            if current_row != Some(*row) {
                current_row = Some(*row);
                row_value_bytes = 0;
            }
            if self.transformation.as_ref().is_some_and(|transformation| {
                matches!(
                    &transformation.transformation,
                    ColumnTransformation::Arrange {
                        source_width,
                        output_columns,
                    } if *column < *source_width && !output_columns.contains(column)
                )
            }) {
                continue;
            }
            row_value_bytes = row_value_bytes.saturating_add(value.len());
            if row_value_bytes > max_record_bytes {
                return Err(record_too_large());
            }
        }

        let Some(transformation) = self.transformation.as_ref() else {
            return (header_clone_bytes <= max_record_bytes)
                .then_some(())
                .ok_or_else(record_too_large);
        };
        match &transformation.transformation {
            ColumnTransformation::Split { output_headers, .. } => {
                if let Some(headers) = output_headers {
                    header_clone_bytes =
                        headers.iter().fold(header_clone_bytes, |length, header| {
                            length.saturating_add(header.len())
                        });
                }
            }
            ColumnTransformation::Join {
                separator,
                output_header,
                ..
            } => {
                if separator.len() > max_record_bytes {
                    return Err(record_too_large());
                }
                if let Some(header) = output_header {
                    header_clone_bytes = header_clone_bytes.saturating_add(header.len());
                }
            }
            ColumnTransformation::Arrange { .. } => {}
        }
        (header_clone_bytes <= max_record_bytes)
            .then_some(())
            .ok_or_else(record_too_large)
    }
}

impl SaveEdits {
    fn prepare(
        mut self,
        has_header: bool,
        replacement: Option<LiteralReplacement>,
        deleted_rows: Vec<RangeInclusive<u64>>,
    ) -> Result<PreparedSaveEdits, QuarryError> {
        if replacement.is_some() && self.transformation.is_some() {
            return Err(QuarryError::InvalidOption(
                "replacement and column transformation cannot run together",
            ));
        }
        validate_deleted_rows(&deleted_rows, u64::from(has_header))?;
        self.cells
            .retain(|(row, _), _| !row_in_deleted_ranges(&deleted_rows, *row));
        Ok(PreparedSaveEdits {
            headers: self.headers,
            cells: self.cells,
            transformation: self
                .transformation
                .map(|transformation| PreparedColumnTransformation::new(transformation, has_header))
                .transpose()?,
            replacement,
            deleted_rows,
        })
    }
}

fn validate_deleted_rows(
    deleted_rows: &[RangeInclusive<u64>],
    data_start: u64,
) -> Result<(), QuarryError> {
    let mut previous_end = None;
    for range in deleted_rows {
        let start = *range.start();
        let end = *range.end();
        if start > end {
            return Err(QuarryError::InvalidOption(
                "deleted row ranges must not be empty",
            ));
        }
        if start < data_start {
            return Err(QuarryError::InvalidOption(
                "deleted rows must target data rows",
            ));
        }
        if previous_end.is_some_and(|previous| start <= previous) {
            return Err(QuarryError::InvalidOption(
                "deleted row ranges must be sorted and non-overlapping",
            ));
        }
        previous_end = Some(end);
    }
    Ok(())
}

fn row_in_deleted_ranges(deleted_rows: &[RangeInclusive<u64>], row: u64) -> bool {
    let index = deleted_rows.partition_point(|range| *range.end() < row);
    deleted_rows
        .get(index)
        .is_some_and(|range| range.contains(&row))
}

impl SaveAsJob {
    fn start_with_edits(
        source_path: PathBuf,
        file_size: u64,
        delimiter: u8,
        has_header: bool,
        edits: SaveEdits,
        target: SaveTarget,
        config: ExportConfig,
    ) -> Result<Self, QuarryError> {
        Self::start_worker_with_deleted_rows(
            source_path,
            file_size,
            delimiter,
            has_header,
            edits,
            None,
            Vec::new(),
            target,
            config,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_worker(
        source_path: PathBuf,
        file_size: u64,
        delimiter: u8,
        has_header: bool,
        edits: SaveEdits,
        replacement: Option<LiteralReplacement>,
        target: SaveTarget,
        config: ExportConfig,
    ) -> Result<Self, QuarryError> {
        Self::start_worker_with_deleted_rows(
            source_path,
            file_size,
            delimiter,
            has_header,
            edits,
            replacement,
            Vec::new(),
            target,
            config,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_worker_with_deleted_rows(
        source_path: PathBuf,
        file_size: u64,
        delimiter: u8,
        has_header: bool,
        edits: SaveEdits,
        replacement: Option<LiteralReplacement>,
        deleted_rows: Vec<RangeInclusive<u64>>,
        target: SaveTarget,
        config: ExportConfig,
    ) -> Result<Self, QuarryError> {
        if config.chunk_bytes == 0 {
            return Err(QuarryError::InvalidOption("save-as chunk must be non-zero"));
        }
        if let Some(replacement) = &replacement {
            if replacement.needle.is_empty() {
                return Err(QuarryError::InvalidOption(
                    "replacement needle must not be empty",
                ));
            }
            if replacement.needle.len() > config.max_record_bytes
                || replacement.replacement.len() > config.max_record_bytes
            {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
        }
        if !has_header && !edits.headers.is_empty() {
            return Err(QuarryError::InvalidOption(
                "header renames require a header row",
            ));
        }
        let data_start = u64::from(has_header);
        if edits
            .cells
            .first_key_value()
            .is_some_and(|((row, _), _)| *row < data_start)
        {
            return Err(QuarryError::InvalidOption(
                "cell edits must target data rows",
            ));
        }
        let edits = edits.prepare(has_header, replacement, deleted_rows)?;
        edits.validate_size_lower_bounds(config.max_record_bytes)?;
        let source = File::open(&source_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
        let output = match target {
            SaveTarget::New(destination, expected) => {
                ExportTarget::new_guarded(&source_path, destination, &source, expected)?
            }
            SaveTarget::WorkingCopy(destination, expected) => {
                ExportTarget::new_private_guarded(&source_path, destination, &source, expected)?
            }
            SaveTarget::Source(expected) => {
                ExportTarget::replace_source(&source_path, source.metadata()?, expected)?
            }
            SaveTarget::Existing {
                destination,
                source_expected,
                destination_expected,
            } => ExportTarget::replace_existing_guarded(
                &source_path,
                destination,
                &source,
                source_expected,
                destination_expected,
            )?,
        };
        let shared = Arc::new(SharedState::new(file_size));
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-save-as".into())
            .spawn(move || {
                let _completion = WorkerCompletion(&worker_state);
                let result = run_save_as(
                    source,
                    output,
                    delimiter,
                    has_header,
                    &edits,
                    config,
                    &worker_state,
                );
                match &result {
                    Ok(SaveWorkerOutcome::Cancelled) => {
                        worker_state.cancelled.store(true, Ordering::Release);
                    }
                    Err(error) => {
                        *worker_state.error.lock().unwrap() = Some(error.to_string());
                    }
                    Ok(SaveWorkerOutcome::Complete { .. } | SaveWorkerOutcome::NoMatch) => {}
                }
                result
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> SaveAsProgress {
        let done = self.shared.done.load(Ordering::Acquire);
        let finished_nanos = self.shared.finished_nanos.load(Ordering::Acquire);
        SaveAsProgress {
            bytes_scanned: self.shared.bytes_scanned.load(Ordering::Acquire),
            bytes_written: self.shared.bytes_written.load(Ordering::Acquire),
            total_bytes: self.shared.total_bytes,
            elapsed: if finished_nanos == 0 {
                self.shared.started.elapsed()
            } else {
                Duration::from_nanos(finished_nanos)
            },
            done,
            cancelled: self.shared.cancelled.load(Ordering::Acquire),
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        if !self.shared.done.load(Ordering::Acquire) {
            self.shared.cancel_requested.store(true, Ordering::Release);
        }
    }

    fn wait_worker(mut self) -> Result<SaveWorkerOutcome, QuarryError> {
        self.handle
            .take()
            .expect("save-as handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }

    pub fn wait(self) -> Result<SaveAsOutcome, QuarryError> {
        match self.wait_worker()? {
            SaveWorkerOutcome::Complete { summary, .. } => Ok(SaveAsOutcome::Complete(summary)),
            SaveWorkerOutcome::Cancelled => Ok(SaveAsOutcome::Cancelled),
            SaveWorkerOutcome::NoMatch => unreachable!("save jobs do not request replacement"),
        }
    }
}

impl Drop for SaveAsJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

impl ReplaceAllJob {
    pub fn progress(&self) -> SaveAsProgress {
        self.inner.progress()
    }

    pub fn error(&self) -> Option<String> {
        self.inner.error()
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub fn wait(self) -> Result<ReplaceAllOutcome, QuarryError> {
        match self.inner.wait_worker()? {
            SaveWorkerOutcome::Complete {
                summary,
                replacements,
            } => Ok(ReplaceAllOutcome::Complete(ReplaceAllSummary {
                destination: summary.destination,
                bytes_written: summary.bytes_written,
                replacements,
            })),
            SaveWorkerOutcome::NoMatch => Ok(ReplaceAllOutcome::NoMatch),
            SaveWorkerOutcome::Cancelled => Ok(ReplaceAllOutcome::Cancelled),
        }
    }
}

impl Session {
    pub fn ensure_source_unchanged(&self) -> Result<(), QuarryError> {
        let source = File::open(&self.path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
        if source_matches_stamp(&source, &self.path, &self.source_stamp)? {
            Ok(())
        } else {
            Err(QuarryError::SourceChanged)
        }
    }

    pub fn start_filtered_export(
        &self,
        query: FilterQuery,
        destination: impl AsRef<Path>,
    ) -> Result<FilterExportJob, QuarryError> {
        FilterExportJob::start(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            query,
            destination.as_ref().to_path_buf(),
            DEFAULT_EXPORT_CONFIG,
        )
    }

    pub fn start_analyze_split(
        &self,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        source_column: usize,
        separator: Vec<u8>,
        max_pieces: usize,
    ) -> Result<SplitAnalysisJob, QuarryError> {
        SplitAnalysisJob::start(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            cell_edits,
            source_column,
            separator,
            max_pieces,
            self.source_stamp.clone(),
            DEFAULT_EXPORT_CONFIG,
        )
    }

    pub fn start_save_as_with_header_renames(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        destination: impl AsRef<Path>,
    ) -> Result<SaveAsJob, QuarryError> {
        self.start_save_as_with_edits(header_renames, BTreeMap::new(), destination)
    }

    pub fn start_save_as_with_edits(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        destination: impl AsRef<Path>,
    ) -> Result<SaveAsJob, QuarryError> {
        self.start_save_as_with_optional_transformation(
            header_renames,
            cell_edits,
            None,
            destination,
        )
    }

    pub fn start_save_as_with_transformation(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        transformation: ColumnTransformation,
        destination: impl AsRef<Path>,
    ) -> Result<SaveAsJob, QuarryError> {
        self.start_save_as_with_optional_transformation(
            header_renames,
            cell_edits,
            Some(transformation),
            destination,
        )
    }

    fn start_save_as_with_optional_transformation(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        transformation: Option<ColumnTransformation>,
        destination: impl AsRef<Path>,
    ) -> Result<SaveAsJob, QuarryError> {
        SaveAsJob::start_with_edits(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            SaveEdits {
                headers: header_renames,
                cells: cell_edits,
                transformation,
            },
            SaveTarget::New(
                destination.as_ref().to_path_buf(),
                self.source_stamp.clone(),
            ),
            DEFAULT_EXPORT_CONFIG,
        )
    }

    pub fn start_create_working_copy(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        transformation: ColumnTransformation,
        destination: impl AsRef<Path>,
    ) -> Result<SaveAsJob, QuarryError> {
        SaveAsJob::start_with_edits(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            SaveEdits {
                headers: header_renames,
                cells: cell_edits,
                transformation: Some(transformation),
            },
            SaveTarget::WorkingCopy(
                destination.as_ref().to_path_buf(),
                self.source_stamp.clone(),
            ),
            DEFAULT_EXPORT_CONFIG,
        )
    }

    pub fn start_create_working_copy_deleting_rows(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        deleted_rows: Vec<RangeInclusive<u64>>,
        destination: impl AsRef<Path>,
    ) -> Result<SaveAsJob, QuarryError> {
        if deleted_rows.is_empty() {
            return Err(QuarryError::InvalidOption(
                "select at least one row to delete",
            ));
        }
        SaveAsJob::start_worker_with_deleted_rows(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            SaveEdits {
                headers: header_renames,
                cells: cell_edits,
                transformation: None,
            },
            None,
            deleted_rows,
            SaveTarget::WorkingCopy(
                destination.as_ref().to_path_buf(),
                self.source_stamp.clone(),
            ),
            DEFAULT_EXPORT_CONFIG,
        )
    }

    pub fn start_create_replaced_working_copy(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        replacement: LiteralReplacement,
        destination: impl AsRef<Path>,
    ) -> Result<ReplaceAllJob, QuarryError> {
        let inner = SaveAsJob::start_worker(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            SaveEdits {
                headers: header_renames,
                cells: cell_edits,
                transformation: None,
            },
            Some(replacement),
            SaveTarget::WorkingCopy(
                destination.as_ref().to_path_buf(),
                self.source_stamp.clone(),
            ),
            DEFAULT_EXPORT_CONFIG,
        )?;
        Ok(ReplaceAllJob { inner })
    }

    pub fn start_save_to_original(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        original: &Session,
    ) -> Result<SaveAsJob, QuarryError> {
        SaveAsJob::start_with_edits(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            SaveEdits {
                headers: header_renames,
                cells: cell_edits,
                transformation: None,
            },
            SaveTarget::Existing {
                destination: original.path.clone(),
                source_expected: self.source_stamp.clone(),
                destination_expected: original.source_stamp.clone(),
            },
            DEFAULT_EXPORT_CONFIG,
        )
        .map_err(|error| match error {
            QuarryError::Io(error) if error.kind() == io::ErrorKind::NotFound => {
                QuarryError::SourceChanged
            }
            error => error,
        })
    }

    pub fn start_save_with_header_renames(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
    ) -> Result<SaveAsJob, QuarryError> {
        self.start_save_with_edits(header_renames, BTreeMap::new())
    }

    pub fn start_save_with_edits(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
    ) -> Result<SaveAsJob, QuarryError> {
        self.start_save_with_optional_transformation(header_renames, cell_edits, None)
    }

    pub fn start_save_with_transformation(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        transformation: ColumnTransformation,
    ) -> Result<SaveAsJob, QuarryError> {
        self.start_save_with_optional_transformation(
            header_renames,
            cell_edits,
            Some(transformation),
        )
    }

    fn start_save_with_optional_transformation(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        transformation: Option<ColumnTransformation>,
    ) -> Result<SaveAsJob, QuarryError> {
        SaveAsJob::start_with_edits(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            SaveEdits {
                headers: header_renames,
                cells: cell_edits,
                transformation,
            },
            SaveTarget::Source(self.source_stamp.clone()),
            DEFAULT_EXPORT_CONFIG,
        )
        .map_err(|error| match error {
            QuarryError::Io(error) if error.kind() == io::ErrorKind::NotFound => {
                QuarryError::SourceChanged
            }
            error => error,
        })
    }
}

enum Publication {
    CreateNew,
    GuardedCreateNew {
        source_path: PathBuf,
        source: File,
        source_stamp: SourceStamp,
    },
    GuardedCreateWorkingCopy {
        source_path: PathBuf,
        source: File,
        source_stamp: SourceStamp,
    },
    ReplaceSource {
        permissions: fs::Permissions,
        source_stamp: SourceStamp,
    },
    GuardedReplaceExisting {
        source_path: PathBuf,
        source: File,
        source_stamp: SourceStamp,
        destination_file: File,
        destination_stamp: SourceStamp,
        permissions: fs::Permissions,
    },
}

enum FirstRecordBomGuard {
    Inactive,
    Probing {
        source_had_bom: bool,
        prefix: Vec<u8>,
    },
    Passthrough {
        inserted_bytes: u64,
    },
    Complete,
}

pub(crate) struct ExportTarget {
    writer: Option<BufWriter<File>>,
    temporary: PathBuf,
    destination: PathBuf,
    publication: Publication,
    first_record_bom_guard: FirstRecordBomGuard,
}

impl ExportTarget {
    fn new(source: &Path, destination: PathBuf) -> Result<Self, QuarryError> {
        validate_destination(source, &destination)?;
        Self::create(destination, Publication::CreateNew)
    }

    fn new_guarded(
        source_path: &Path,
        destination: PathBuf,
        source: &File,
        source_stamp: SourceStamp,
    ) -> Result<Self, QuarryError> {
        validate_destination(source_path, &destination)?;
        if !source_matches_stamp(source, source_path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        Self::create(
            destination,
            Publication::GuardedCreateNew {
                source_path: source_path.to_path_buf(),
                source: source.try_clone()?,
                source_stamp,
            },
        )
    }

    pub(crate) fn new_private_guarded(
        source_path: &Path,
        destination: PathBuf,
        source: &File,
        source_stamp: SourceStamp,
    ) -> Result<Self, QuarryError> {
        validate_destination(source_path, &destination)?;
        if !source_matches_stamp(source, source_path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        Self::create(
            destination,
            Publication::GuardedCreateWorkingCopy {
                source_path: source_path.to_path_buf(),
                source: source.try_clone()?,
                source_stamp,
            },
        )
    }

    fn replace_source(
        source: &Path,
        metadata: fs::Metadata,
        expected: SourceStamp,
    ) -> Result<Self, QuarryError> {
        let path_metadata = fs::symlink_metadata(source)?;
        if path_metadata.file_type().is_symlink() {
            return Err(QuarryError::InvalidOption(
                "saving through a symbolic link is not supported; use Save As instead",
            ));
        }
        let source_stamp = SourceStamp::from_metadata(&metadata);
        if SourceStamp::from_metadata(&path_metadata) != expected || source_stamp != expected {
            return Err(QuarryError::SourceChanged);
        }
        Self::create(
            source.to_path_buf(),
            Publication::ReplaceSource {
                permissions: metadata.permissions(),
                source_stamp,
            },
        )
    }

    fn replace_existing_guarded(
        source_path: &Path,
        destination: PathBuf,
        source: &File,
        source_stamp: SourceStamp,
        destination_stamp: SourceStamp,
    ) -> Result<Self, QuarryError> {
        validate_distinct_paths(source_path, &destination)?;
        if !source_matches_stamp(source, source_path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }

        let path_metadata = fs::symlink_metadata(&destination).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
        if path_metadata.file_type().is_symlink() {
            return Err(QuarryError::InvalidOption(
                "saving through a symbolic link is not supported; use Save As instead",
            ));
        }
        let destination_file = File::open(&destination).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
        if SourceStamp::from_metadata(&path_metadata) != destination_stamp
            || SourceStamp::from_metadata(&destination_file.metadata()?) != destination_stamp
        {
            return Err(QuarryError::SourceChanged);
        }
        Self::create(
            destination,
            Publication::GuardedReplaceExisting {
                source_path: source_path.to_path_buf(),
                source: source.try_clone()?,
                source_stamp,
                destination_file,
                destination_stamp,
                permissions: path_metadata.permissions(),
            },
        )
    }

    fn create(destination: PathBuf, publication: Publication) -> Result<Self, QuarryError> {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        destination.file_name().ok_or(QuarryError::InvalidOption(
            "export destination must name a file",
        ))?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        match &publication {
            Publication::GuardedCreateWorkingCopy { .. } => {
                options.mode(0o600);
            }
            Publication::ReplaceSource { permissions, .. }
            | Publication::GuardedReplaceExisting { permissions, .. } => {
                options.mode(permissions.mode());
            }
            Publication::CreateNew | Publication::GuardedCreateNew { .. } => {}
        }
        for _ in 0..100 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(".quarry-export-{}-{id}.tmp", std::process::id()));
            match options.open(&temporary) {
                Ok(file) => {
                    #[cfg(unix)]
                    let permissions = match &publication {
                        Publication::GuardedCreateWorkingCopy { .. } => {
                            Some(fs::Permissions::from_mode(0o600))
                        }
                        Publication::ReplaceSource { permissions, .. }
                        | Publication::GuardedReplaceExisting { permissions, .. } => {
                            Some(permissions.clone())
                        }
                        Publication::CreateNew | Publication::GuardedCreateNew { .. } => None,
                    };
                    #[cfg(not(unix))]
                    let permissions: Option<fs::Permissions> = match &publication {
                        Publication::ReplaceSource { permissions, .. }
                        | Publication::GuardedReplaceExisting { permissions, .. } => {
                            Some(permissions.clone())
                        }
                        Publication::CreateNew
                        | Publication::GuardedCreateNew { .. }
                        | Publication::GuardedCreateWorkingCopy { .. } => None,
                    };
                    if let Some(permissions) = permissions
                        && let Err(error) = file.set_permissions(permissions)
                    {
                        drop(file);
                        let _ = fs::remove_file(&temporary);
                        return Err(error.into());
                    }
                    return Ok(Self {
                        writer: Some(BufWriter::new(file)),
                        temporary,
                        destination,
                        publication,
                        first_record_bom_guard: FirstRecordBomGuard::Inactive,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a temporary export file",
        )
        .into())
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), QuarryError> {
        let writer = self.writer.as_mut().expect("export writer is present");
        match &mut self.first_record_bom_guard {
            FirstRecordBomGuard::Inactive | FirstRecordBomGuard::Complete => {
                writer.write_all(bytes)?;
            }
            FirstRecordBomGuard::Probing {
                source_had_bom: true,
                ..
            } if !bytes.is_empty() => {
                writer.write_all(UTF8_BOM)?;
                writer.write_all(bytes)?;
                self.first_record_bom_guard = FirstRecordBomGuard::Passthrough {
                    inserted_bytes: UTF8_BOM.len() as u64,
                };
            }
            FirstRecordBomGuard::Probing {
                source_had_bom: true,
                ..
            } => {}
            FirstRecordBomGuard::Probing {
                source_had_bom: false,
                prefix,
            } => {
                let mut consumed = 0_usize;
                while consumed < bytes.len() && prefix.len() < UTF8_BOM.len() {
                    prefix.push(bytes[consumed]);
                    consumed += 1;
                    if !UTF8_BOM.starts_with(prefix) {
                        writer.write_all(prefix)?;
                        writer.write_all(&bytes[consumed..])?;
                        self.first_record_bom_guard =
                            FirstRecordBomGuard::Passthrough { inserted_bytes: 0 };
                        return Ok(());
                    }
                }
                if prefix.len() == UTF8_BOM.len() {
                    writer.write_all(UTF8_BOM)?;
                    writer.write_all(prefix)?;
                    writer.write_all(&bytes[consumed..])?;
                    self.first_record_bom_guard = FirstRecordBomGuard::Passthrough {
                        inserted_bytes: UTF8_BOM.len() as u64,
                    };
                }
            }
            FirstRecordBomGuard::Passthrough { .. } => writer.write_all(bytes)?,
        }
        Ok(())
    }

    fn begin_first_record_bom_guard(&mut self, source_had_bom: bool) {
        debug_assert!(matches!(
            self.first_record_bom_guard,
            FirstRecordBomGuard::Inactive
        ));
        self.first_record_bom_guard = FirstRecordBomGuard::Probing {
            source_had_bom,
            prefix: Vec::with_capacity(UTF8_BOM.len()),
        };
    }

    fn finish_first_record_bom_guard(&mut self) -> Result<u64, QuarryError> {
        let guard = std::mem::replace(
            &mut self.first_record_bom_guard,
            FirstRecordBomGuard::Complete,
        );
        match guard {
            FirstRecordBomGuard::Probing { prefix, .. } => {
                self.writer
                    .as_mut()
                    .expect("export writer is present")
                    .write_all(&prefix)?;
                Ok(0)
            }
            FirstRecordBomGuard::Passthrough { inserted_bytes } => Ok(inserted_bytes),
            FirstRecordBomGuard::Inactive => {
                self.first_record_bom_guard = FirstRecordBomGuard::Inactive;
                Ok(0)
            }
            FirstRecordBomGuard::Complete => Ok(0),
        }
    }

    fn ensure_source_unchanged(&self) -> Result<(), QuarryError> {
        let unchanged = match &self.publication {
            Publication::CreateNew => true,
            Publication::GuardedCreateNew {
                source_path,
                source,
                source_stamp,
            }
            | Publication::GuardedCreateWorkingCopy {
                source_path,
                source,
                source_stamp,
            }
            | Publication::GuardedReplaceExisting {
                source_path,
                source,
                source_stamp,
                ..
            } => source_matches_stamp(source, source_path, source_stamp)?,
            Publication::ReplaceSource { source_stamp, .. } => {
                fs::symlink_metadata(&self.destination)
                    .ok()
                    .filter(|metadata| !metadata.file_type().is_symlink())
                    .is_some_and(|metadata| SourceStamp::from_metadata(&metadata) == *source_stamp)
            }
        };
        unchanged.then_some(()).ok_or(QuarryError::SourceChanged)
    }

    pub(crate) fn publish(
        mut self,
        rows_written: u64,
        bytes_written: u64,
        cancel_requested: &AtomicBool,
    ) -> Result<FilterExportOutcome, QuarryError> {
        if cancel_requested.load(Ordering::Acquire) {
            drop(self.writer.take());
            self.remove_temporary()?;
            return Ok(FilterExportOutcome::Cancelled);
        }
        let mut writer = self.writer.take().expect("export writer is present");
        writer.flush()?;
        let file = writer.into_inner().map_err(|error| error.into_error())?;
        match &self.publication {
            Publication::ReplaceSource { permissions, .. }
            | Publication::GuardedReplaceExisting { permissions, .. } => {
                file.set_permissions(permissions.clone())?;
            }
            #[cfg(unix)]
            Publication::GuardedCreateWorkingCopy { .. } => {
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            Publication::CreateNew | Publication::GuardedCreateNew { .. } => {}
            #[cfg(not(unix))]
            Publication::GuardedCreateWorkingCopy { .. } => {}
        }
        file.sync_all()?;
        drop(file);
        if cancel_requested.load(Ordering::Acquire) {
            self.remove_temporary()?;
            return Ok(FilterExportOutcome::Cancelled);
        }
        if let Err(error) = self.ensure_source_unchanged() {
            self.remove_temporary()?;
            return Err(error);
        }
        let publish_result = match &self.publication {
            Publication::CreateNew | Publication::GuardedCreateNew { .. } => {
                publish_no_replace(&self.temporary, &self.destination)
            }
            Publication::GuardedCreateWorkingCopy { .. } => {
                if cancel_requested.load(Ordering::Acquire) {
                    self.remove_temporary()?;
                    return Ok(FilterExportOutcome::Cancelled);
                }
                publish_no_replace(&self.temporary, &self.destination)
            }
            Publication::ReplaceSource { .. } => {
                if cancel_requested.load(Ordering::Acquire) {
                    self.remove_temporary()?;
                    return Ok(FilterExportOutcome::Cancelled);
                }
                fs::rename(&self.temporary, &self.destination)
            }
            Publication::GuardedReplaceExisting {
                destination_file,
                destination_stamp,
                ..
            } => {
                let destination_unchanged = fs::symlink_metadata(&self.destination)
                    .ok()
                    .filter(|metadata| !metadata.file_type().is_symlink())
                    .is_some_and(|metadata| {
                        SourceStamp::from_metadata(&metadata) == *destination_stamp
                    })
                    && SourceStamp::from_metadata(&destination_file.metadata()?)
                        == *destination_stamp;
                if !destination_unchanged {
                    self.remove_temporary()?;
                    return Err(QuarryError::SourceChanged);
                }
                if cancel_requested.load(Ordering::Acquire) {
                    self.remove_temporary()?;
                    return Ok(FilterExportOutcome::Cancelled);
                }
                fs::rename(&self.temporary, &self.destination)
            }
        };
        if let Err(error) = publish_result {
            self.remove_temporary()?;
            return Err(if error.kind() == io::ErrorKind::AlreadyExists {
                QuarryError::ExportDestinationExists
            } else {
                error.into()
            });
        }
        let _ = self.remove_temporary();
        Ok(FilterExportOutcome::Complete(FilterExportSummary {
            destination: self.destination.clone(),
            rows_written,
            bytes_written,
        }))
    }

    fn discard(mut self) -> Result<(), QuarryError> {
        drop(self.writer.take());
        self.remove_temporary()
    }

    fn remove_temporary(&self) -> Result<(), QuarryError> {
        match fs::remove_file(&self.temporary) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for ExportTarget {
    fn drop(&mut self) {
        drop(self.writer.take());
        let _ = fs::remove_file(&self.temporary);
    }
}

pub(crate) fn source_matches_stamp(
    source: &File,
    source_path: &Path,
    expected: &SourceStamp,
) -> Result<bool, QuarryError> {
    if SourceStamp::from_metadata(&source.metadata()?) != *expected {
        return Ok(false);
    }
    let path_metadata = match fs::metadata(source_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(SourceStamp::from_metadata(&path_metadata) == *expected)
}

fn validate_destination(source: &Path, destination: &Path) -> Result<(), QuarryError> {
    validate_distinct_paths(source, destination)?;
    match fs::symlink_metadata(destination) {
        Ok(_) => Err(QuarryError::ExportDestinationExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_distinct_paths(source: &Path, destination: &Path) -> Result<(), QuarryError> {
    let current_dir = std::env::current_dir()?;
    if normalize_path(source, &current_dir) == normalize_path(destination, &current_dir) {
        return Err(QuarryError::ExportDestinationIsSource);
    }
    let same_path = fs::canonicalize(source)
        .ok()
        .zip(fs::canonicalize(destination).ok())
        .is_some_and(|(source, destination)| source == destination);
    if same_path {
        Err(QuarryError::ExportDestinationIsSource)
    } else {
        Ok(())
    }
}

fn normalize_path(path: &Path, current_dir: &Path) -> PathBuf {
    let mut normalized = if path.is_absolute() {
        PathBuf::new()
    } else {
        current_dir.to_path_buf()
    };
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[cfg(target_os = "macos")]
fn publish_no_replace(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    const AT_FDCWD: c_int = -2;
    const RENAME_EXCL: c_uint = 0x0000_0004;

    unsafe extern "C" {
        fn renameatx_np(
            from_fd: c_int,
            from: *const c_char,
            to_fd: c_int,
            to: *const c_char,
            flags: c_uint,
        ) -> c_int;
    }

    let temporary = CString::new(temporary.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "temporary path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both pointers remain valid NUL-terminated strings for the duration of the call.
    let result = unsafe {
        renameatx_np(
            AT_FDCWD,
            temporary.as_ptr(),
            AT_FDCWD,
            destination.as_ptr(),
            RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn publish_no_replace(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::hard_link(temporary, destination)
}

enum ScanOutcome {
    Complete {
        rows_written: u64,
        bytes_written: u64,
    },
    Cancelled,
}

fn run_export(
    mut source: File,
    mut output: ExportTarget,
    delimiter: u8,
    data_start: u64,
    query: &FilterQuery,
    config: ExportConfig,
    shared: &SharedState,
) -> Result<FilterExportOutcome, QuarryError> {
    match scan_export(
        &mut source,
        &mut output,
        delimiter,
        data_start,
        query,
        config,
        shared,
    ) {
        Ok(ScanOutcome::Complete {
            rows_written,
            bytes_written,
        }) => output.publish(rows_written, bytes_written, &shared.cancel_requested),
        Ok(ScanOutcome::Cancelled) => {
            output.discard()?;
            Ok(FilterExportOutcome::Cancelled)
        }
        Err(error) => {
            output.discard()?;
            Err(error)
        }
    }
}

struct SaveCopySummary {
    bytes_written: u64,
    replacements: u64,
}

fn run_save_as(
    mut source: File,
    mut output: ExportTarget,
    delimiter: u8,
    has_header: bool,
    edits: &PreparedSaveEdits,
    config: ExportConfig,
    shared: &SharedState,
) -> Result<SaveWorkerOutcome, QuarryError> {
    let copied = if edits.cells.is_empty()
        && edits.transformation.is_none()
        && edits.replacement.is_none()
        && edits.deleted_rows.is_empty()
        && has_header
    {
        copy_with_rewritten_header(
            &mut source,
            &mut output,
            delimiter,
            &edits.headers,
            config,
            shared,
        )
        .map(|result| {
            result.map(|bytes_written| SaveCopySummary {
                bytes_written,
                replacements: 0,
            })
        })
    } else {
        copy_with_rewritten_records(
            &mut source,
            &mut output,
            delimiter,
            has_header,
            edits,
            config,
            shared,
        )
    };
    match copied {
        Ok(Some(summary)) if edits.replacement.is_some() && summary.replacements == 0 => {
            if shared.cancel_requested.load(Ordering::Acquire) {
                output.discard()?;
                return Ok(SaveWorkerOutcome::Cancelled);
            }
            let source_guard = output.ensure_source_unchanged();
            output.discard()?;
            source_guard?;
            Ok(SaveWorkerOutcome::NoMatch)
        }
        Ok(Some(summary)) => {
            match output.publish(
                summary.replacements,
                summary.bytes_written,
                &shared.cancel_requested,
            )? {
                FilterExportOutcome::Complete(published) => Ok(SaveWorkerOutcome::Complete {
                    summary: SaveAsSummary {
                        destination: published.destination,
                        bytes_written: published.bytes_written,
                    },
                    replacements: published.rows_written,
                }),
                FilterExportOutcome::Cancelled => Ok(SaveWorkerOutcome::Cancelled),
            }
        }
        Ok(None) => {
            output.discard()?;
            Ok(SaveWorkerOutcome::Cancelled)
        }
        Err(error) => {
            output.discard()?;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_split_analysis(
    mut source: File,
    source_path: &Path,
    source_stamp: &SourceStamp,
    delimiter: u8,
    has_header: bool,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    source_column: usize,
    separator: &[u8],
    max_pieces: usize,
    config: ExportConfig,
    shared: &SharedState,
) -> Result<SplitAnalysisOutcome, QuarryError> {
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut chunk = vec![0; config.chunk_bytes];
    let mut record = Vec::new();
    let mut absolute_start = 0_u64;
    let mut row_number = 0_u64;
    let mut rows_scanned = 0_u64;
    let mut observed_max_pieces = 1_usize;
    let data_start = u64::from(has_header);
    let finder = Finder::new(separator);

    loop {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(SplitAnalysisOutcome::Cancelled);
        }
        let read = source.read(&mut chunk)?;
        if read == 0 {
            let mut deferred_error = None;
            let mut cancelled = false;
            let finish_result = scanner.finish(absolute_start, |_| {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    cancelled = true;
                } else {
                    match analyze_split_record(
                        &record,
                        row_number,
                        data_start,
                        delimiter,
                        cell_edits,
                        source_column,
                        &finder,
                        separator.len(),
                        max_pieces,
                        config.max_record_bytes,
                        &mut observed_max_pieces,
                    ) {
                        Ok(true) => rows_scanned = rows_scanned.saturating_add(1),
                        Ok(false) => {}
                        Err(error) => deferred_error = Some(error),
                    }
                }
                record.clear();
                row_number = row_number.saturating_add(1);
            });
            shared.rows_scanned.store(rows_scanned, Ordering::Release);
            if cancelled {
                return Ok(SplitAnalysisOutcome::Cancelled);
            }
            if let Some(error) = deferred_error {
                return Err(error);
            }
            finish_result?;
            if has_header && row_number == 0 {
                return Err(QuarryError::InvalidOption(
                    "source does not contain a header row",
                ));
            }
            if cell_edits
                .keys()
                .any(|(row, column)| *column == source_column && *row >= row_number)
            {
                return Err(QuarryError::InvalidOption("cell edit row is out of range"));
            }
            if !source_matches_stamp(&source, source_path, source_stamp)? {
                return Err(QuarryError::SourceChanged);
            }
            return Ok(SplitAnalysisOutcome::Complete(SplitAnalysisSummary {
                rows_scanned,
                max_pieces: observed_max_pieces,
            }));
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
                        match analyze_split_record(
                            &record,
                            row_number,
                            data_start,
                            delimiter,
                            cell_edits,
                            source_column,
                            &finder,
                            separator.len(),
                            max_pieces,
                            config.max_record_bytes,
                            &mut observed_max_pieces,
                        ) {
                            Ok(true) => rows_scanned = rows_scanned.saturating_add(1),
                            Ok(false) => {}
                            Err(error) => deferred_error = Some(error),
                        }
                    }
                }
            }
            record.clear();
            row_number = row_number.saturating_add(1);
            segment_start = local_end;
        });

        absolute_start = absolute_start.saturating_add(read as u64);
        shared
            .bytes_scanned
            .store(absolute_start, Ordering::Release);
        shared.rows_scanned.store(rows_scanned, Ordering::Release);
        if cancelled || shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(SplitAnalysisOutcome::Cancelled);
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
fn analyze_split_record(
    record: &[u8],
    row_number: u64,
    data_start: u64,
    delimiter: u8,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    source_column: usize,
    finder: &Finder<'_>,
    separator_len: usize,
    max_pieces: usize,
    max_record_bytes: usize,
    observed_max_pieces: &mut usize,
) -> Result<bool, QuarryError> {
    if row_number < data_start {
        return Ok(false);
    }
    let record = if row_number == 0 {
        record.strip_prefix(UTF8_BOM).unwrap_or(record)
    } else {
        record
    };
    let fields = parse_record_with_field_limit(record, delimiter, MAX_TRANSFORMATION_COLUMNS)
        .map_err(|error| match error.kind {
            ParseErrorKind::FieldLimitExceeded(_) => {
                QuarryError::InvalidOption("source record column count exceeds the supported limit")
            }
            _ => error.into(),
        })?;
    let edited = cell_edits.get(&(row_number, source_column));
    if edited.is_some() && source_column >= fields.len() {
        return Err(QuarryError::InvalidOption(
            "cell edit column is out of range",
        ));
    }
    let value = edited.map_or_else(
        || {
            fields
                .get(source_column)
                .map_or(&[][..], |field| field.as_ref())
        },
        Vec::as_slice,
    );
    if value.len() > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }

    let mut pieces = 1_usize;
    let mut remainder = value;
    while let Some(position) = finder.find(remainder) {
        pieces = pieces.saturating_add(1);
        if pieces > max_pieces {
            return Err(QuarryError::InvalidOption(
                "split result exceeds the supported column limit",
            ));
        }
        remainder = &remainder[position + separator_len..];
    }
    *observed_max_pieces = (*observed_max_pieces).max(pieces);
    Ok(true)
}

fn copy_with_rewritten_records(
    source: &mut File,
    output: &mut ExportTarget,
    delimiter: u8,
    has_header: bool,
    edits: &PreparedSaveEdits,
    config: ExportConfig,
    shared: &SharedState,
) -> Result<Option<SaveCopySummary>, QuarryError> {
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut chunk = vec![0; config.chunk_bytes];
    let mut record = Vec::new();
    let mut absolute_start = 0_u64;
    let mut row_number = 0_u64;
    let mut bytes_written = 0_u64;
    let mut replacements = 0_u64;
    let mut next_cell_edit_row = edits.cells.first_key_value().map(|((row, _), _)| *row);
    let mut deleted_range_index = 0_usize;
    let guard_first_retained_record = !has_header && row_in_deleted_ranges(&edits.deleted_rows, 0);
    let source_had_bom = if guard_first_retained_record {
        source_starts_with_utf8_bom(source)?
    } else {
        false
    };
    let mut first_retained_record_pending = guard_first_retained_record;
    let rewrites_every_record = edits.transformation.is_some() || edits.replacement.is_some();

    loop {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        let read = source.read(&mut chunk)?;
        if read == 0 {
            let mut deferred_error = None;
            let mut cancelled = false;
            let finish_result = scanner.finish(absolute_start, |_| {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    cancelled = true;
                } else if deleted_row(&edits.deleted_rows, &mut deleted_range_index, row_number) {
                    // The scanner still validates the record boundary, but selected rows
                    // never enter the output or the bounded rewrite buffer.
                } else if rewrites_every_record
                    || row_number == 0 && !edits.headers.is_empty()
                    || next_cell_edit_row == Some(row_number)
                {
                    let has_cell_edits = next_cell_edit_row == Some(row_number);
                    begin_first_retained_record_bom_guard(
                        output,
                        &mut first_retained_record_pending,
                        source_had_bom,
                    );
                    match write_saved_record(
                        output,
                        &record,
                        row_number,
                        delimiter,
                        has_header,
                        edits,
                        config.max_record_bytes,
                    ) {
                        Ok(written) => match output.finish_first_record_bom_guard() {
                            Ok(marker_bytes) => {
                                bytes_written = bytes_written
                                    .saturating_add(marker_bytes)
                                    .saturating_add(written.bytes_written);
                                replacements = replacements.saturating_add(written.replacements);
                            }
                            Err(error) => deferred_error = Some(error),
                        },
                        Err(error) => deferred_error = Some(error),
                    }
                    if has_cell_edits {
                        next_cell_edit_row = row_number.checked_add(1).and_then(|next_row| {
                            edits
                                .cells
                                .range((next_row, 0)..)
                                .next()
                                .map(|((row, _), _)| *row)
                        });
                    }
                } else {
                    begin_first_retained_record_bom_guard(
                        output,
                        &mut first_retained_record_pending,
                        source_had_bom,
                    );
                    match output.finish_first_record_bom_guard() {
                        Ok(marker_bytes) => {
                            bytes_written = bytes_written.saturating_add(marker_bytes)
                        }
                        Err(error) => deferred_error = Some(error),
                    }
                }
                row_number += 1;
            });
            shared.bytes_written.store(bytes_written, Ordering::Release);
            if cancelled {
                return Ok(None);
            }
            if let Some(error) = deferred_error {
                return Err(error);
            }
            finish_result?;
            if has_header && rewrites_every_record && row_number == 0 {
                return Err(QuarryError::InvalidOption(
                    "source does not contain a header row",
                ));
            }
            if next_cell_edit_row.is_some() {
                return Err(QuarryError::InvalidOption("cell edit row is out of range"));
            }
            if edits
                .deleted_rows
                .last()
                .is_some_and(|range| *range.end() >= row_number)
            {
                return Err(QuarryError::InvalidOption("deleted row is out of range"));
            }
            return Ok(Some(SaveCopySummary {
                bytes_written,
                replacements,
            }));
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
                    let has_cell_edits = next_cell_edit_row == Some(row_number);
                    if deleted_row(&edits.deleted_rows, &mut deleted_range_index, row_number) {
                    } else if rewrites_every_record
                        || row_number == 0 && !edits.headers.is_empty()
                        || has_cell_edits
                    {
                        record.extend_from_slice(&chunk[segment_start..local_end]);
                        begin_first_retained_record_bom_guard(
                            output,
                            &mut first_retained_record_pending,
                            source_had_bom,
                        );
                        match write_saved_record(
                            output,
                            &record,
                            row_number,
                            delimiter,
                            has_header,
                            edits,
                            config.max_record_bytes,
                        ) {
                            Ok(written) => match output.finish_first_record_bom_guard() {
                                Ok(marker_bytes) => {
                                    bytes_written = bytes_written
                                        .saturating_add(marker_bytes)
                                        .saturating_add(written.bytes_written);
                                    replacements =
                                        replacements.saturating_add(written.replacements);
                                }
                                Err(error) => deferred_error = Some(error),
                            },
                            Err(error) => deferred_error = Some(error),
                        }
                    } else {
                        let raw = &chunk[segment_start..local_end];
                        begin_first_retained_record_bom_guard(
                            output,
                            &mut first_retained_record_pending,
                            source_had_bom,
                        );
                        match output
                            .write_all(raw)
                            .and_then(|()| output.finish_first_record_bom_guard())
                        {
                            Ok(marker_bytes) => {
                                bytes_written = bytes_written
                                    .saturating_add(raw.len() as u64)
                                    .saturating_add(marker_bytes)
                            }
                            Err(error) => deferred_error = Some(error),
                        }
                    }
                    if has_cell_edits {
                        next_cell_edit_row = row_number.checked_add(1).and_then(|next_row| {
                            edits
                                .cells
                                .range((next_row, 0)..)
                                .next()
                                .map(|((row, _), _)| *row)
                        });
                    }
                }
            }
            record.clear();
            row_number += 1;
            segment_start = local_end;
        });

        absolute_start = absolute_start.saturating_add(read as u64);
        shared
            .bytes_scanned
            .store(absolute_start, Ordering::Release);
        if cancelled || shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        if let Some(error) = deferred_error {
            return Err(error);
        }
        scan_result?;
        let trailing = &chunk[segment_start..read];
        if deleted_row(&edits.deleted_rows, &mut deleted_range_index, row_number) {
        } else if rewrites_every_record
            || row_number == 0 && !edits.headers.is_empty()
            || next_cell_edit_row == Some(row_number)
        {
            record.extend_from_slice(trailing);
            if record.len() > config.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
        } else {
            begin_first_retained_record_bom_guard(
                output,
                &mut first_retained_record_pending,
                source_had_bom,
            );
            output.write_all(trailing)?;
            bytes_written = bytes_written.saturating_add(trailing.len() as u64);
        }
        shared.bytes_written.store(bytes_written, Ordering::Release);
    }
}

fn deleted_row(deleted_rows: &[RangeInclusive<u64>], range_index: &mut usize, row: u64) -> bool {
    while deleted_rows
        .get(*range_index)
        .is_some_and(|range| *range.end() < row)
    {
        *range_index += 1;
    }
    deleted_rows
        .get(*range_index)
        .is_some_and(|range| range.contains(&row))
}

fn source_starts_with_utf8_bom(source: &mut File) -> Result<bool, QuarryError> {
    let position = source.stream_position()?;
    let mut prefix = [0_u8; UTF8_BOM.len()];
    let mut read = 0_usize;
    while read < prefix.len() {
        let count = source.read(&mut prefix[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    source.seek(SeekFrom::Start(position))?;
    Ok(read == UTF8_BOM.len() && prefix == UTF8_BOM)
}

fn begin_first_retained_record_bom_guard(
    output: &mut ExportTarget,
    pending: &mut bool,
    source_had_bom: bool,
) {
    if *pending {
        output.begin_first_record_bom_guard(source_had_bom);
        *pending = false;
    }
}

fn write_saved_record(
    output: &mut ExportTarget,
    record: &[u8],
    row_number: u64,
    delimiter: u8,
    has_header: bool,
    edits: &PreparedSaveEdits,
    max_record_bytes: usize,
) -> Result<SaveCopySummary, QuarryError> {
    if record.len() > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }
    if edits.transformation.is_some() {
        return write_transformed_record(
            output,
            record,
            row_number,
            delimiter,
            has_header && row_number == 0,
            edits,
            max_record_bytes,
        )
        .map(|bytes_written| SaveCopySummary {
            bytes_written,
            replacements: 0,
        });
    }
    if let Some(replacement) = &edits.replacement {
        if has_header && row_number == 0 {
            let bytes_written = if edits.headers.is_empty() {
                output.write_all(record)?;
                record.len() as u64
            } else {
                write_rewritten_header(output, record, delimiter, &edits.headers, max_record_bytes)?
            };
            return Ok(SaveCopySummary {
                bytes_written,
                replacements: 0,
            });
        }
        return write_replaced_data_record(
            output,
            record,
            row_number,
            delimiter,
            &edits.cells,
            replacement,
            max_record_bytes,
        );
    }
    if row_number == 0 && !edits.headers.is_empty() {
        return write_rewritten_header(output, record, delimiter, &edits.headers, max_record_bytes)
            .map(|bytes_written| SaveCopySummary {
                bytes_written,
                replacements: 0,
            });
    }
    let has_cell_edits = edits
        .cells
        .range((row_number, 0)..=(row_number, usize::MAX))
        .next()
        .is_some();
    if has_cell_edits {
        return write_rewritten_data_record(
            output,
            record,
            row_number,
            delimiter,
            &edits.cells,
            max_record_bytes,
        )
        .map(|bytes_written| SaveCopySummary {
            bytes_written,
            replacements: 0,
        });
    }
    output.write_all(record)?;
    Ok(SaveCopySummary {
        bytes_written: record.len() as u64,
        replacements: 0,
    })
}

fn write_replaced_data_record(
    output: &mut ExportTarget,
    record: &[u8],
    row_number: u64,
    delimiter: u8,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    replacement: &LiteralReplacement,
    max_record_bytes: usize,
) -> Result<SaveCopySummary, QuarryError> {
    if record.len() > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }
    let original = record;
    let (prefix, record) = if row_number == 0 {
        record
            .strip_prefix(UTF8_BOM)
            .map_or((&[][..], record), |record| (UTF8_BOM, record))
    } else {
        (&[][..], record)
    };
    let ending = if record.ends_with(b"\r\n") {
        b"\r\n".as_slice()
    } else if record.ends_with(b"\n") {
        b"\n".as_slice()
    } else {
        b"".as_slice()
    };
    let fields = parse_record(record, delimiter)?;
    let row_edits = cell_edits.range((row_number, 0)..=(row_number, usize::MAX));
    if row_edits
        .clone()
        .next_back()
        .is_some_and(|((_, column), _)| *column >= fields.len())
    {
        return Err(QuarryError::InvalidOption(
            "cell edit column is out of range",
        ));
    }
    let has_cell_edits = row_edits.clone().next().is_some();
    let mut replacements = 0_u64;
    let mut decoded_bytes = 0_usize;
    let mut replaced_fields = Vec::with_capacity(fields.len());
    for (column, field) in fields.iter().enumerate() {
        let value = cell_edits
            .get(&(row_number, column))
            .map_or_else(|| field.as_ref(), Vec::as_slice);
        let (value, count) = replace_literal(value, replacement, max_record_bytes)?;
        decoded_bytes = decoded_bytes.saturating_add(value.len());
        if decoded_bytes > max_record_bytes {
            return Err(QuarryError::RecordTooLarge {
                limit: max_record_bytes,
            });
        }
        replacements = replacements.saturating_add(count);
        replaced_fields.push(value);
    }

    if replacements == 0 && !has_cell_edits {
        output.write_all(original)?;
        return Ok(SaveCopySummary {
            bytes_written: original.len() as u64,
            replacements: 0,
        });
    }
    let bytes_written = write_serialized_fields(
        output,
        prefix,
        &replaced_fields,
        delimiter,
        ending,
        row_number == 0,
        max_record_bytes,
    )?;
    Ok(SaveCopySummary {
        bytes_written,
        replacements,
    })
}

fn replace_literal(
    field: &[u8],
    replacement: &LiteralReplacement,
    max_record_bytes: usize,
) -> Result<(Vec<u8>, u64), QuarryError> {
    let matcher = ByteMatcher::new(&replacement.needle, replacement.case_sensitivity);
    let mut count = 0_usize;
    let mut remainder = field;
    while let Some(position) = matcher.find(remainder) {
        count = count.saturating_add(1);
        remainder = &remainder[position + replacement.needle.len()..];
    }
    if count == 0 {
        return Ok((field.to_vec(), 0));
    }
    let output_len = field
        .len()
        .saturating_sub(replacement.needle.len().saturating_mul(count))
        .saturating_add(replacement.replacement.len().saturating_mul(count));
    if output_len > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }

    let mut output = Vec::with_capacity(output_len);
    let mut remainder = field;
    while let Some(position) = matcher.find(remainder) {
        output.extend_from_slice(&remainder[..position]);
        output.extend_from_slice(&replacement.replacement);
        remainder = &remainder[position + replacement.needle.len()..];
    }
    output.extend_from_slice(remainder);
    Ok((output, count as u64))
}

fn write_transformed_record(
    output: &mut ExportTarget,
    record: &[u8],
    row_number: u64,
    delimiter: u8,
    is_header: bool,
    edits: &PreparedSaveEdits,
    max_record_bytes: usize,
) -> Result<u64, QuarryError> {
    let (prefix, record) = if row_number == 0 {
        record
            .strip_prefix(UTF8_BOM)
            .map_or((&[][..], record), |record| (UTF8_BOM, record))
    } else {
        (&[][..], record)
    };
    let ending = if record.ends_with(b"\r\n") {
        b"\r\n".as_slice()
    } else if record.ends_with(b"\n") {
        b"\n".as_slice()
    } else {
        b"".as_slice()
    };
    let mut fields = parse_record_with_field_limit(record, delimiter, MAX_TRANSFORMATION_COLUMNS)
        .map_err(|error| match error.kind {
            ParseErrorKind::FieldLimitExceeded(_) => {
                QuarryError::InvalidOption("source record column count exceeds the supported limit")
            }
            _ => error.into(),
        })?
        .into_iter()
        .map(|field| field.into_owned())
        .collect::<Vec<_>>();

    if is_header {
        if edits
            .headers
            .last_key_value()
            .is_some_and(|(column, _)| *column >= fields.len())
        {
            return Err(QuarryError::InvalidOption(
                "header rename column is out of range",
            ));
        }
        for (column, value) in &edits.headers {
            if !edits.transformation.as_ref().is_some_and(|transformation| {
                transformation
                    .transformation
                    .replaces_source_column(*column)
            }) {
                fields[*column] = value.clone();
            }
        }
    } else {
        let row_edits = edits
            .cells
            .range((row_number, 0)..=(row_number, usize::MAX));
        if row_edits
            .clone()
            .next_back()
            .is_some_and(|((_, column), _)| *column >= fields.len())
        {
            return Err(QuarryError::InvalidOption(
                "cell edit column is out of range",
            ));
        }
        for ((_, column), value) in row_edits {
            fields[*column] = value.clone();
        }
    }

    let transformation = edits
        .transformation
        .as_ref()
        .expect("transformed records have a transformation");
    let fields = if is_header {
        transformation.transform_header_fields(fields)?
    } else {
        transformation.transform_fields(fields, max_record_bytes)?
    };
    write_serialized_fields(
        output,
        prefix,
        &fields,
        delimiter,
        ending,
        row_number == 0,
        max_record_bytes,
    )
}

fn write_serialized_fields(
    output: &mut ExportTarget,
    prefix: &[u8],
    fields: &[Vec<u8>],
    delimiter: u8,
    ending: &[u8],
    is_first_record: bool,
    max_record_bytes: usize,
) -> Result<u64, QuarryError> {
    let serialized_len = fields.iter().enumerate().fold(
        prefix
            .len()
            .saturating_add(ending.len())
            .saturating_add(fields.len().saturating_sub(1)),
        |length, (column, field)| {
            let force_quotes = (is_first_record
                && prefix.is_empty()
                && column == 0
                && field.starts_with(UTF8_BOM))
                || (fields.len() == 1 && field.is_empty() && ending.is_empty());
            length.saturating_add(delimited_field_len(field, delimiter, force_quotes))
        },
    );
    if serialized_len > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }

    output.write_all(prefix)?;
    let mut bytes_written = prefix.len() as u64;
    for (column, field) in fields.iter().enumerate() {
        if column > 0 {
            output.write_all(&[delimiter])?;
            bytes_written = bytes_written.saturating_add(1);
        }
        let force_quotes =
            (is_first_record && prefix.is_empty() && column == 0 && field.starts_with(UTF8_BOM))
                || (fields.len() == 1 && field.is_empty() && ending.is_empty());
        bytes_written = bytes_written.saturating_add(write_delimited_field(
            output,
            field,
            delimiter,
            force_quotes,
        )?);
    }
    output.write_all(ending)?;
    Ok(bytes_written.saturating_add(ending.len() as u64))
}

fn copy_with_rewritten_header(
    source: &mut File,
    output: &mut ExportTarget,
    delimiter: u8,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    config: ExportConfig,
    shared: &SharedState,
) -> Result<Option<u64>, QuarryError> {
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut chunk = vec![0; config.chunk_bytes];
    let mut header = Vec::new();
    let mut absolute_start = 0_u64;
    let mut bytes_written = 0_u64;
    let mut header_written = false;

    loop {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        let read = source.read(&mut chunk)?;
        if read == 0 {
            if !header_written {
                let mut found_header = false;
                scanner.finish(absolute_start, |_| found_header = true)?;
                if !found_header {
                    return Err(QuarryError::InvalidOption(
                        "source does not contain a header row",
                    ));
                }
                bytes_written = write_rewritten_header(
                    output,
                    &header,
                    delimiter,
                    header_renames,
                    config.max_record_bytes,
                )?;
                shared.bytes_written.store(bytes_written, Ordering::Release);
            }
            return Ok(Some(bytes_written));
        }

        absolute_start = absolute_start.saturating_add(read as u64);
        shared
            .bytes_scanned
            .store(absolute_start, Ordering::Release);

        if header_written {
            output.write_all(&chunk[..read])?;
            bytes_written = bytes_written.saturating_add(read as u64);
            shared.bytes_written.store(bytes_written, Ordering::Release);
            continue;
        }

        let chunk_start = absolute_start - read as u64;
        let mut segment_start = 0_usize;
        while segment_start < read {
            if shared.cancel_requested.load(Ordering::Acquire) {
                return Ok(None);
            }
            let segment_end = memchr(b'\n', &chunk[segment_start..read])
                .map_or(read, |relative| segment_start + relative + 1);
            let mut found_header = false;
            scanner.scan_chunk(
                &chunk[segment_start..segment_end],
                chunk_start + segment_start as u64,
                |_| found_header = true,
            )?;
            header.extend_from_slice(&chunk[segment_start..segment_end]);
            if header.len() > config.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
            segment_start = segment_end;

            if found_header {
                bytes_written = write_rewritten_header(
                    output,
                    &header,
                    delimiter,
                    header_renames,
                    config.max_record_bytes,
                )?;
                if segment_start < read {
                    output.write_all(&chunk[segment_start..read])?;
                    bytes_written = bytes_written.saturating_add((read - segment_start) as u64);
                }
                shared.bytes_written.store(bytes_written, Ordering::Release);
                header_written = true;
                break;
            }
        }
    }
}

fn write_rewritten_header(
    output: &mut ExportTarget,
    record: &[u8],
    delimiter: u8,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    max_record_bytes: usize,
) -> Result<u64, QuarryError> {
    write_rewritten_record(
        output,
        record,
        delimiter,
        header_renames.last_key_value().map(|(column, _)| *column),
        |column| header_renames.get(&column).map(Vec::as_slice),
        None,
        max_record_bytes,
    )
}

fn write_rewritten_data_record(
    output: &mut ExportTarget,
    record: &[u8],
    row_number: u64,
    delimiter: u8,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    max_record_bytes: usize,
) -> Result<u64, QuarryError> {
    let last_column = cell_edits
        .range((row_number, 0)..=(row_number, usize::MAX))
        .next_back()
        .map(|((_, column), _)| *column);
    write_rewritten_record(
        output,
        record,
        delimiter,
        last_column,
        |column| cell_edits.get(&(row_number, column)).map(Vec::as_slice),
        Some(row_number),
        max_record_bytes,
    )
}

fn write_rewritten_record<'a>(
    output: &mut ExportTarget,
    record: &[u8],
    delimiter: u8,
    last_replacement_column: Option<usize>,
    replacement: impl Fn(usize) -> Option<&'a [u8]>,
    data_row: Option<u64>,
    max_record_bytes: usize,
) -> Result<u64, QuarryError> {
    let (prefix, record) = if data_row.is_none_or(|row| row == 0) {
        record
            .strip_prefix(UTF8_BOM)
            .map_or((&[][..], record), |record| (UTF8_BOM, record))
    } else {
        (&[][..], record)
    };
    let fields = parse_record(record, delimiter)?;
    if last_replacement_column.is_some_and(|column| column >= fields.len()) {
        return Err(QuarryError::InvalidOption(if data_row.is_some() {
            "cell edit column is out of range"
        } else {
            "header rename column is out of range"
        }));
    }

    let ending = if record.ends_with(b"\r\n") {
        b"\r\n".as_slice()
    } else if record.ends_with(b"\n") {
        b"\n".as_slice()
    } else {
        b"".as_slice()
    };
    let mut serialized_len = prefix
        .len()
        .saturating_add(ending.len())
        .saturating_add(fields.len().saturating_sub(1));
    for (column, field) in fields.iter().enumerate() {
        let field = replacement(column).unwrap_or_else(|| field.as_ref());
        let force_quotes = (data_row.is_none_or(|row| row == 0)
            && prefix.is_empty()
            && column == 0
            && field.starts_with(UTF8_BOM))
            || (fields.len() == 1 && field.is_empty() && ending.is_empty());
        serialized_len =
            serialized_len.saturating_add(delimited_field_len(field, delimiter, force_quotes));
    }
    if serialized_len > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }

    output.write_all(prefix)?;
    let mut bytes_written = prefix.len() as u64;
    for (column, field) in fields.iter().enumerate() {
        if column > 0 {
            output.write_all(&[delimiter])?;
            bytes_written += 1;
        }
        let field = replacement(column).unwrap_or_else(|| field.as_ref());
        let force_quotes = (data_row.is_none_or(|row| row == 0)
            && prefix.is_empty()
            && column == 0
            && field.starts_with(UTF8_BOM))
            || (fields.len() == 1 && field.is_empty() && ending.is_empty());
        bytes_written = bytes_written.saturating_add(write_delimited_field(
            output,
            field,
            delimiter,
            force_quotes,
        )?);
    }

    output.write_all(ending)?;
    Ok(bytes_written.saturating_add(ending.len() as u64))
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

fn write_delimited_field(
    output: &mut ExportTarget,
    field: &[u8],
    delimiter: u8,
    force_quotes: bool,
) -> Result<u64, QuarryError> {
    let needs_quotes = force_quotes
        || field
            .iter()
            .any(|byte| matches!(*byte, b'"' | b'\r' | b'\n') || *byte == delimiter);
    if !needs_quotes {
        output.write_all(field)?;
        return Ok(field.len() as u64);
    }

    output.write_all(b"\"")?;
    let mut start = 0_usize;
    let mut bytes_written = 2_u64;
    while let Some(relative) = memchr(b'"', &field[start..]) {
        let quote = start + relative;
        output.write_all(&field[start..quote])?;
        output.write_all(b"\"\"")?;
        bytes_written = bytes_written.saturating_add((quote - start + 2) as u64);
        start = quote + 1;
    }
    output.write_all(&field[start..])?;
    output.write_all(b"\"")?;
    Ok(bytes_written.saturating_add((field.len() - start) as u64))
}

fn scan_export(
    source: &mut File,
    output: &mut ExportTarget,
    delimiter: u8,
    data_start: u64,
    query: &FilterQuery,
    config: ExportConfig,
    shared: &SharedState,
) -> Result<ScanOutcome, QuarryError> {
    let matchers: Vec<_> = query
        .predicates
        .iter()
        .map(|predicate| ByteMatcher::new(&predicate.value, query.case_sensitivity))
        .collect();
    let groups = predicate_groups(query);
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut chunk = vec![0; config.chunk_bytes];
    let mut absolute_start = 0_u64;
    let mut row_number = 0_u64;
    let mut rows_scanned = 0_u64;
    let mut rows_written = 0_u64;
    let mut bytes_written = 0_u64;
    let mut record = Vec::new();

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
                } else if let Err(error) = process_record(
                    &record,
                    row_number,
                    data_start,
                    delimiter,
                    query,
                    &matchers,
                    &groups,
                    config.max_record_bytes,
                    output,
                    &mut rows_written,
                    &mut bytes_written,
                ) {
                    deferred_error = Some(error);
                }
                rows_scanned += 1;
            });
            publish_progress(
                shared,
                absolute_start,
                rows_scanned,
                rows_written,
                bytes_written,
            );
            if cancelled {
                return Ok(ScanOutcome::Cancelled);
            }
            if let Some(error) = deferred_error {
                return Err(error);
            }
            finish_result?;
            return Ok(ScanOutcome::Complete {
                rows_written,
                bytes_written,
            });
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
                    if let Err(error) = process_record(
                        &record,
                        row_number,
                        data_start,
                        delimiter,
                        query,
                        &matchers,
                        &groups,
                        config.max_record_bytes,
                        output,
                        &mut rows_written,
                        &mut bytes_written,
                    ) {
                        deferred_error = Some(error);
                    }
                }
            }
            record.clear();
            row_number += 1;
            rows_scanned += 1;
            segment_start = local_end;
        });

        absolute_start += read as u64;
        publish_progress(
            shared,
            absolute_start,
            rows_scanned,
            rows_written,
            bytes_written,
        );
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
fn process_record(
    record: &[u8],
    row_number: u64,
    data_start: u64,
    delimiter: u8,
    query: &FilterQuery,
    matchers: &[ByteMatcher<'_>],
    groups: &[Vec<usize>],
    max_record_bytes: usize,
    output: &mut ExportTarget,
    rows_written: &mut u64,
    bytes_written: &mut u64,
) -> Result<(), QuarryError> {
    if record.len() > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }
    if row_number < data_start
        || matching_fields(record, delimiter, row_number, query, matchers, groups)?.is_some()
    {
        output.write_all(record)?;
        *bytes_written = bytes_written.saturating_add(record.len() as u64);
        if row_number >= data_start {
            *rows_written += 1;
        }
    }
    Ok(())
}

fn publish_progress(
    shared: &SharedState,
    bytes_scanned: u64,
    rows_scanned: u64,
    rows_written: u64,
    bytes_written: u64,
) {
    shared.bytes_scanned.store(bytes_scanned, Ordering::Release);
    shared.rows_scanned.store(rows_scanned, Ordering::Release);
    shared.rows_written.store(rows_written, Ordering::Release);
    shared.bytes_written.store(bytes_written, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        ColumnTransformation, ExportConfig, ExportTarget, FilterExportJob, FilterExportOutcome,
        LiteralReplacement, MAX_TRANSFORMATION_COLUMNS, ReplaceAllJob, ReplaceAllOutcome,
        SaveAsJob, SaveAsOutcome, SaveEdits, SaveTarget, SharedState, SplitAnalysisJob,
        SplitAnalysisOutcome, WorkerCompletion, split_field,
    };
    use crate::{
        CaseSensitivity, FilterOperator, FilterQuery, HeaderMode, IndexConfig, OpenOptions,
        QuarryError, Session,
    };

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn path(name: &str) -> std::path::PathBuf {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("quarry-export-{}-{id}-{name}", std::process::id()))
    }

    fn fixture(bytes: &[u8]) -> std::path::PathBuf {
        let directory = path("case");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("source.csv");
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    fn destination(source: &std::path::Path, name: &str) -> std::path::PathBuf {
        source.parent().unwrap().join(name)
    }

    fn remove_case(source: &std::path::Path) {
        fs::remove_dir(source.parent().unwrap()).unwrap();
    }

    fn session(path: &std::path::Path, delimiter: u8, header_mode: HeaderMode) -> Session {
        Session::open(
            path,
            OpenOptions {
                delimiter: Some(delimiter),
                header_mode,
                ..OpenOptions::default()
            },
        )
        .unwrap()
    }

    fn query() -> FilterQuery {
        FilterQuery::single_with_case(
            2,
            FilterOperator::Equals,
            b"keep".to_vec(),
            CaseSensitivity::Insensitive,
        )
    }

    fn temporary_exports(destination: &std::path::Path) -> Vec<std::path::PathBuf> {
        let parent = destination.parent().unwrap();
        let prefix = format!(".quarry-export-{}-", std::process::id());
        fs::read_dir(parent)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            })
            .collect()
    }

    fn wait_until_done(job: &FilterExportJob) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !job.progress().done {
            assert!(Instant::now() < deadline, "export did not finish promptly");
            thread::yield_now();
        }
    }

    fn wait_until_save_done(job: &SaveAsJob) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !job.progress().done {
            assert!(Instant::now() < deadline, "save-as did not finish promptly");
            thread::yield_now();
        }
    }

    fn wait_until_replace_done(job: &ReplaceAllJob) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !job.progress().done {
            assert!(
                Instant::now() < deadline,
                "replace-all did not finish promptly"
            );
            thread::yield_now();
        }
    }

    fn wait_until_split_analysis_done(job: &SplitAnalysisJob) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !job.progress().done {
            assert!(
                Instant::now() < deadline,
                "split analysis did not finish promptly"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn transformation_helpers_split_remainders_join_in_order_and_fill_ragged_sources() {
        let split = ColumnTransformation::Split {
            source_column: 1,
            separator: b"::".to_vec(),
            output_count: 3,
            output_headers: Some(vec![
                b"first".to_vec(),
                b"middle".to_vec(),
                b"last".to_vec(),
            ]),
        };
        assert_eq!(
            split
                .transform_fields(&[b"1".to_vec(), b"Ada::Augusta::Lovelace::Byron".to_vec()])
                .unwrap(),
            vec![
                b"1".to_vec(),
                b"Ada".to_vec(),
                b"Augusta".to_vec(),
                b"Lovelace::Byron".to_vec(),
            ]
        );
        assert_eq!(
            split
                .transform_header_fields(&[b"id".to_vec(), b"name".to_vec()])
                .unwrap(),
            vec![
                b"id".to_vec(),
                b"first".to_vec(),
                b"middle".to_vec(),
                b"last".to_vec(),
            ]
        );

        let join = ColumnTransformation::Join {
            source_columns: vec![3, 0],
            separator: b", ".to_vec(),
            output_header: Some(b"city and first".to_vec()),
        };
        assert_eq!(
            join.transform_fields(&[b"Grace".to_vec(), Vec::new(), b"85".to_vec()])
                .unwrap(),
            vec![b", Grace".to_vec(), Vec::new(), b"85".to_vec()]
        );
        assert_eq!(
            join.transform_header_fields(&[
                b"first".to_vec(),
                b"last".to_vec(),
                b"age".to_vec(),
                b"city".to_vec(),
            ])
            .unwrap(),
            vec![
                b"city and first".to_vec(),
                b"last".to_vec(),
                b"age".to_vec(),
            ]
        );

        let maximum_width = vec![Vec::new(); MAX_TRANSFORMATION_COLUMNS];
        assert!(matches!(
            split.transform_fields(&maximum_width),
            Err(QuarryError::InvalidOption(_))
        ));
        let excessive_width = vec![Vec::new(); MAX_TRANSFORMATION_COLUMNS + 1];
        assert!(matches!(
            join.transform_fields(&excessive_width),
            Err(QuarryError::InvalidOption(_))
        ));
    }

    #[test]
    fn arrange_reorders_drops_pads_and_preserves_undiscovered_trailing_fields() {
        let arrange = ColumnTransformation::Arrange {
            source_width: 4,
            output_columns: vec![2, 0],
        };

        assert_eq!(
            arrange
                .transform_fields(&[
                    b"one".to_vec(),
                    b"two".to_vec(),
                    b"three".to_vec(),
                    b"four".to_vec(),
                ])
                .unwrap(),
            [b"three".to_vec(), b"one".to_vec()]
        );
        assert_eq!(
            arrange.transform_header_fields(&[b"id".to_vec()]).unwrap(),
            [Vec::new(), b"id".to_vec()]
        );
        assert_eq!(
            arrange
                .transform_fields(&[
                    b"1".to_vec(),
                    b"Ada".to_vec(),
                    b"London".to_vec(),
                    b"drop".to_vec(),
                    b"unknown one".to_vec(),
                    b"unknown two".to_vec(),
                ])
                .unwrap(),
            [
                b"London".to_vec(),
                b"1".to_vec(),
                b"unknown one".to_vec(),
                b"unknown two".to_vec(),
            ]
        );
    }

    #[test]
    fn arrange_rejects_empty_duplicate_and_out_of_range_layouts() {
        for transformation in [
            ColumnTransformation::Arrange {
                source_width: 0,
                output_columns: vec![0],
            },
            ColumnTransformation::Arrange {
                source_width: 2,
                output_columns: Vec::new(),
            },
            ColumnTransformation::Arrange {
                source_width: 2,
                output_columns: vec![0, 0],
            },
            ColumnTransformation::Arrange {
                source_width: 2,
                output_columns: vec![2],
            },
            ColumnTransformation::Arrange {
                source_width: MAX_TRANSFORMATION_COLUMNS + 1,
                output_columns: vec![0],
            },
        ] {
            assert!(matches!(
                transformation.transform_fields(&[b"value".to_vec()]),
                Err(QuarryError::InvalidOption(_))
            ));
        }
    }

    #[test]
    fn row_deletion_preserves_csv_fidelity_and_sparse_edits() {
        let source_bytes = b"\xEF\xBB\xBFid,note\r\n1,keep\r\n2,\"delete\nme\"\r\n3,edit\r\n4,last";
        let source = fixture(source_bytes);
        let source_session = session(&source, b',', HeaderMode::FirstRow);
        let working_path = destination(&source, "rows-deleted.csv");
        let job = source_session
            .start_create_working_copy_deleting_rows(
                BTreeMap::new(),
                BTreeMap::from([((2, 1), b"ignored".to_vec()), ((3, 1), b"edited".to_vec())]),
                vec![2..=2, 4..=4],
                &working_path,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(
            fs::read(&working_path).unwrap(),
            b"\xEF\xBB\xBFid,note\r\n1,keep\r\n3,edited\r\n"
        );
        assert!(temporary_exports(&working_path).is_empty());
        let reopened = session(&working_path, b',', HeaderMode::FirstRow);
        assert_eq!(reopened.first_rows.len(), 3);

        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);
    }

    #[test]
    fn row_deletion_handles_headerless_bom_and_rejects_invalid_rows() {
        let source_bytes = b"\xEF\xBB\xBFdrop,first\r\nkeep,second\r\n";
        let source = fixture(source_bytes);
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "first-row-deleted.csv");
        let job = source_session
            .start_create_working_copy_deleting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                vec![0..=0],
                &working_path,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(
            fs::read(&working_path).unwrap(),
            b"\xEF\xBB\xBFkeep,second\r\n"
        );
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        fs::remove_file(working_path).unwrap();

        let headered = session(&source, b',', HeaderMode::FirstRow);
        let invalid_path = destination(&source, "invalid-row.csv");
        assert!(matches!(
            headered.start_create_working_copy_deleting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                vec![0..=0],
                &invalid_path,
            ),
            Err(QuarryError::InvalidOption(_))
        ));
        assert!(!invalid_path.exists());

        let out_of_range_path = destination(&source, "out-of-range-row.csv");
        let job = source_session
            .start_create_working_copy_deleting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                vec![9..=9],
                &out_of_range_path,
            )
            .unwrap();
        wait_until_save_done(&job);
        assert!(matches!(job.wait(), Err(QuarryError::InvalidOption(_))));
        assert!(!out_of_range_path.exists());
        assert!(temporary_exports(&out_of_range_path).is_empty());
        assert_eq!(fs::read(&source).unwrap(), source_bytes);

        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn row_deletion_defers_and_disambiguates_headerless_bom() {
        let bom_only_record = b"\xEF\xBB\xBFonly\n";
        let source = fixture(bom_only_record);
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "all-rows-deleted.csv");
        let job = source_session
            .start_create_working_copy_deleting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                vec![0..=0],
                &working_path,
            )
            .unwrap();

        wait_until_save_done(&job);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("row deletion unexpectedly cancelled");
        };
        assert_eq!(summary.bytes_written, 0);
        assert!(fs::read(&working_path).unwrap().is_empty());
        assert_eq!(fs::read(&source).unwrap(), bom_only_record);
        fs::remove_file(working_path).unwrap();
        fs::remove_file(&source).unwrap();
        remove_case(&source);

        let source_bytes = b"drop\n\xEF\xBB\xBFkeep,value\n";
        let source = fixture(source_bytes);
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "leading-bom-data.csv");
        let job = SaveAsJob::start_worker_with_deleted_rows(
            source.clone(),
            source_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: None,
            },
            None,
            vec![0..=0],
            SaveTarget::WorkingCopy(working_path.clone(), source_session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: 64,
            },
        )
        .unwrap();

        wait_until_save_done(&job);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("row deletion unexpectedly cancelled");
        };
        assert_eq!(
            fs::read(&working_path).unwrap(),
            b"\xEF\xBB\xBF\xEF\xBB\xBFkeep,value\n"
        );
        assert_eq!(
            summary.bytes_written,
            fs::metadata(&working_path).unwrap().len()
        );
        let reopened = session(&working_path, b',', HeaderMode::NoHeader);
        assert_eq!(reopened.first_rows[0].fields[0], b"\xEF\xBB\xBFkeep");
        assert_eq!(fs::read(&source).unwrap(), source_bytes);

        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);
    }

    #[test]
    fn row_deletion_bom_guard_covers_rewritten_and_partial_prefixes() {
        let source_bytes = b"drop\nplain,value\n";
        let source = fixture(source_bytes);
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "rewritten-leading-bom.csv");
        let job = SaveAsJob::start_worker_with_deleted_rows(
            source.clone(),
            source_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::from([((1, 0), b"\xEF\xBB\xBFedited".to_vec())]),
                transformation: None,
            },
            None,
            vec![0..=0],
            SaveTarget::WorkingCopy(working_path.clone(), source_session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: 64,
            },
        )
        .unwrap();

        wait_until_save_done(&job);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("row deletion unexpectedly cancelled");
        };
        assert_eq!(
            fs::read(&working_path).unwrap(),
            b"\xEF\xBB\xBF\xEF\xBB\xBFedited,value\n"
        );
        assert_eq!(
            summary.bytes_written,
            fs::metadata(&working_path).unwrap().len()
        );
        let reopened = session(&working_path, b',', HeaderMode::NoHeader);
        assert_eq!(reopened.first_rows[0].fields[0], b"\xEF\xBB\xBFedited");
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);

        let partial_prefix = b"drop\n\xEF\xBB";
        let source = fixture(partial_prefix);
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "partial-bom-prefix.csv");
        let job = SaveAsJob::start_worker_with_deleted_rows(
            source.clone(),
            source_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: None,
            },
            None,
            vec![0..=0],
            SaveTarget::WorkingCopy(working_path.clone(), source_session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: 64,
            },
        )
        .unwrap();

        wait_until_save_done(&job);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("row deletion unexpectedly cancelled");
        };
        assert_eq!(fs::read(&working_path).unwrap(), b"\xEF\xBB");
        assert_eq!(summary.bytes_written, 2);
        assert_eq!(fs::read(&source).unwrap(), partial_prefix);
        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);
    }

    #[test]
    fn row_deletion_skips_large_selected_records_without_buffering() {
        let mut source_bytes = vec![b'x'; 1024];
        source_bytes.extend_from_slice(b"\nkeep\n");
        let source = fixture(&source_bytes);
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "large-row-deleted.csv");
        let job = SaveAsJob::start_worker_with_deleted_rows(
            source.clone(),
            source_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: None,
            },
            None,
            vec![0..=0],
            SaveTarget::WorkingCopy(working_path.clone(), source_session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 3,
                max_record_bytes: 16,
            },
        )
        .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&working_path).unwrap(), b"keep\n");
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(temporary_exports(&working_path).is_empty());

        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);
    }

    #[test]
    fn cancelling_row_deletion_preserves_source_and_removes_private_output() {
        let source_bytes = vec![b'a'; 4 * 1024 * 1024];
        let source = fixture(&source_bytes);
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "cancelled-row-deletion.csv");
        let job = SaveAsJob::start_worker_with_deleted_rows(
            source.clone(),
            source_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: None,
            },
            None,
            vec![0..=0],
            SaveTarget::WorkingCopy(working_path.clone(), source_session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: 16,
            },
        )
        .unwrap();

        assert_eq!(temporary_exports(&working_path).len(), 1);
        job.cancel();
        wait_until_save_done(&job);
        assert_eq!(job.wait().unwrap(), SaveAsOutcome::Cancelled);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!working_path.exists());
        assert!(temporary_exports(&working_path).is_empty());

        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn row_deletion_does_not_publish_when_source_changes_during_copy() {
        let source_bytes = vec![b'a'; 4 * 1024 * 1024];
        let source = fixture(&source_bytes);
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "conflicted-row-deletion.csv");
        let job = SaveAsJob::start_worker_with_deleted_rows(
            source.clone(),
            source_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: None,
            },
            None,
            vec![0..=0],
            SaveTarget::WorkingCopy(working_path.clone(), source_session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: 16,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while job.progress().bytes_scanned < 100 {
            assert!(
                !job.progress().done,
                "row deletion completed before source change"
            );
            assert!(Instant::now() < deadline, "row deletion made no progress");
            thread::yield_now();
        }

        let external = b"external,change\n";
        fs::write(&source, external).unwrap();
        wait_until_save_done(&job);
        assert!(matches!(job.wait(), Err(QuarryError::SourceChanged)));
        assert_eq!(fs::read(&source).unwrap(), external);
        assert!(!working_path.exists());
        assert!(temporary_exports(&working_path).is_empty());

        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn arrange_working_copy_applies_overlays_before_projection_and_cleans_staging() {
        let source_bytes = b"id,name\n1,Ada,London,tail\n2,Grace\n";
        let expected = b",account\nParis,10,tail\n,2\n";
        let source = fixture(source_bytes);
        let original = session(&source, b',', HeaderMode::FirstRow);
        let working_path = destination(&source, "arranged-working.csv");
        let job = original
            .start_create_working_copy(
                BTreeMap::from([(0, b"account".to_vec())]),
                BTreeMap::from([((1, 0), b"10".to_vec()), ((1, 2), b"Paris".to_vec())]),
                ColumnTransformation::Arrange {
                    source_width: 3,
                    output_columns: vec![2, 0],
                },
                &working_path,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&working_path).unwrap(), expected);
        assert!(temporary_exports(&working_path).is_empty());

        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);
    }

    #[test]
    fn arrange_preflight_ignores_oversized_edits_to_dropped_columns() {
        let source = fixture(b"a,b");
        let destination = destination(&source, "arranged.csv");
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            source_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::from([((0, 1), vec![b'x'; 9])]),
                transformation: Some(ColumnTransformation::Arrange {
                    source_width: 2,
                    output_columns: vec![0],
                }),
            },
            SaveTarget::New(destination.clone(), source_session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: 3,
            },
        )
        .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&destination).unwrap(), b"a");
        assert!(temporary_exports(&destination).is_empty());

        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn replace_all_ignores_case_applies_edits_first_and_skips_headers() {
        let source_bytes = b"first,second,third\r\n\"ABA aBa\",before,tail\r\nnone,\"aBA,Aba\"\r\n";
        let expected = b"aba header,second,third\r\nX X,XbA,tail\r\nnone,\"X,X\"\r\n";
        let source = fixture(source_bytes);
        let source_session = session(&source, b',', HeaderMode::FirstRow);
        let working_path = destination(&source, "replaced-working.csv");
        let job = source_session
            .start_create_replaced_working_copy(
                BTreeMap::from([(0, b"aba header".to_vec())]),
                BTreeMap::from([((1, 1), b"AbAbA".to_vec())]),
                LiteralReplacement {
                    needle: b"aba".to_vec(),
                    replacement: b"X".to_vec(),
                    case_sensitivity: CaseSensitivity::Insensitive,
                },
                &working_path,
            )
            .unwrap();

        wait_until_replace_done(&job);
        let ReplaceAllOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("replacement should publish a working copy");
        };
        assert_eq!(summary.destination, working_path);
        assert_eq!(summary.replacements, 5);
        assert_eq!(summary.bytes_written, expected.len() as u64);
        assert_eq!(fs::read(&working_path).unwrap(), expected);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(temporary_exports(&working_path).is_empty());

        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);
    }

    #[test]
    fn replace_all_no_match_discards_overlays_staging_and_destination() {
        let source_bytes = b"label,value\nnone,here\n";
        let source = fixture(source_bytes);
        let source_session = session(&source, b',', HeaderMode::FirstRow);
        let working_path = destination(&source, "no-match-working.csv");
        let job = source_session
            .start_create_replaced_working_copy(
                BTreeMap::from([(0, b"changed".to_vec())]),
                BTreeMap::from([((1, 1), b"still here".to_vec())]),
                LiteralReplacement {
                    needle: b"absent".to_vec(),
                    replacement: b"replacement".to_vec(),
                    case_sensitivity: CaseSensitivity::Sensitive,
                },
                &working_path,
            )
            .unwrap();

        wait_until_replace_done(&job);
        assert_eq!(job.wait().unwrap(), ReplaceAllOutcome::NoMatch);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!working_path.exists());
        assert!(temporary_exports(&working_path).is_empty());

        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn replace_all_enforces_output_record_limit_without_publication() {
        let source = fixture(b"x,x\n");
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "oversized-replacement.csv");
        let inner = SaveAsJob::start_worker(
            source.clone(),
            source_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: None,
            },
            Some(LiteralReplacement {
                needle: b"x".to_vec(),
                replacement: b"12345678".to_vec(),
                case_sensitivity: CaseSensitivity::Sensitive,
            }),
            SaveTarget::WorkingCopy(working_path.clone(), source_session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: 12,
            },
        )
        .unwrap();
        let job = ReplaceAllJob { inner };

        wait_until_replace_done(&job);
        assert!(matches!(
            job.wait(),
            Err(QuarryError::RecordTooLarge { limit: 12 })
        ));
        assert!(!working_path.exists());
        assert!(temporary_exports(&working_path).is_empty());

        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn cancelling_replace_all_discards_private_output() {
        let source_bytes = b"hit,value\n".repeat(500_000);
        let source = fixture(&source_bytes);
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "cancelled-replacement.csv");
        let inner = SaveAsJob::start_worker(
            source.clone(),
            source_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: None,
            },
            Some(LiteralReplacement {
                needle: b"hit".to_vec(),
                replacement: b"matched".to_vec(),
                case_sensitivity: CaseSensitivity::Sensitive,
            }),
            SaveTarget::WorkingCopy(working_path.clone(), source_session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
            },
        )
        .unwrap();
        let job = ReplaceAllJob { inner };
        job.cancel();
        wait_until_replace_done(&job);
        assert!(job.progress().cancelled);
        assert_eq!(job.wait().unwrap(), ReplaceAllOutcome::Cancelled);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!working_path.exists());
        assert!(temporary_exports(&working_path).is_empty());

        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn split_blank_header_constructor_keeps_the_current_header_then_inserts_blanks() {
        let split = ColumnTransformation::split_with_blank_headers(
            1,
            b"@".to_vec(),
            3,
            Some(b"email".to_vec()),
        )
        .unwrap();

        assert_eq!(
            split
                .transform_header_fields(&[b"id".to_vec(), b"email".to_vec(), b"city".to_vec(),])
                .unwrap(),
            vec![
                b"id".to_vec(),
                b"email".to_vec(),
                Vec::new(),
                Vec::new(),
                b"city".to_vec(),
            ]
        );
        assert_eq!(
            split
                .transform_fields(&[
                    b"1".to_vec(),
                    b"local@example@tail".to_vec(),
                    b"Boston".to_vec(),
                ])
                .unwrap(),
            vec![
                b"1".to_vec(),
                b"local".to_vec(),
                b"example".to_vec(),
                b"tail".to_vec(),
                b"Boston".to_vec(),
            ]
        );
    }

    #[test]
    fn split_analysis_derives_width_from_data_after_sparse_cell_edits() {
        let source = fixture(b"email@header@must@not@count,city\none@two,Boston\nplain,Chicago\n");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = session
            .start_analyze_split(
                BTreeMap::from([((2, 0), b"local@domain@tail".to_vec())]),
                0,
                b"@".to_vec(),
                8,
            )
            .unwrap();

        wait_until_split_analysis_done(&job);
        let progress = job.progress();
        let SplitAnalysisOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("split analysis unexpectedly cancelled");
        };
        assert_eq!(summary.rows_scanned, 2);
        assert_eq!(summary.max_pieces, 3);
        assert_eq!(progress.rows_scanned, 2);
        assert_eq!(progress.bytes_scanned, fs::metadata(&source).unwrap().len());

        let job = session
            .start_analyze_split(BTreeMap::new(), 0, b"@".to_vec(), 1)
            .unwrap();
        wait_until_split_analysis_done(&job);
        assert!(matches!(job.wait(), Err(QuarryError::InvalidOption(_))));

        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn split_analysis_cancels_in_the_background() {
        let source = fixture(&vec![b'a'; 4 * 1024 * 1024]);
        let session = session(&source, b',', HeaderMode::NoHeader);
        let job = SplitAnalysisJob::start(
            source.clone(),
            session.file_size,
            b',',
            false,
            BTreeMap::new(),
            0,
            b"@".to_vec(),
            8,
            session.source_stamp.clone(),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
            },
        )
        .unwrap();

        job.cancel();
        wait_until_split_analysis_done(&job);
        assert!(job.progress().cancelled);
        assert_eq!(job.wait().unwrap(), SplitAnalysisOutcome::Cancelled);

        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn split_skips_searching_when_the_separator_is_longer_than_the_field() {
        assert_eq!(
            split_field(b"value", &vec![b'x'; 4_096], 4),
            vec![b"value".to_vec(), Vec::new(), Vec::new(), Vec::new()]
        );
    }

    #[test]
    fn save_as_streams_split_after_cell_and_header_edits() {
        let source_bytes =
            b"\xEF\xBB\xBFid;full;note\r\n1;Ada::Augusta::Lovelace;\"old\nnote\"\r\n2;Grace;tail";
        let expected = b"\xEF\xBB\xBFid;first;middle;last;memo\r\n1;Ada;Augusta;Lovelace;\"old\nnote\"\r\n2;Grace;Brewster;Hopper;tail";
        let source = fixture(source_bytes);
        let destination = destination(&source, "split.csv");
        let session = session(&source, b';', HeaderMode::FirstRow);
        let job = session
            .start_save_as_with_transformation(
                BTreeMap::from([(2, b"memo".to_vec())]),
                BTreeMap::from([((2, 1), b"Grace::Brewster::Hopper".to_vec())]),
                ColumnTransformation::Split {
                    source_column: 1,
                    separator: b"::".to_vec(),
                    output_count: 3,
                    output_headers: Some(vec![
                        b"first".to_vec(),
                        b"middle".to_vec(),
                        b"last".to_vec(),
                    ]),
                },
                &destination,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert_eq!(job.progress().bytes_scanned, source_bytes.len() as u64);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("split save-as unexpectedly cancelled");
        };
        assert_eq!(summary.bytes_written, expected.len() as u64);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn private_working_copy_reopens_and_saves_its_current_arrangement_to_original() {
        let source_bytes = b"id,email,city\n1,ada@example.com,London\n2,plain,Paris\n";
        let materialized = b"id,email,,city\n1,ada,example.com,London\n2,plain,,Paris\n";
        let saved = b"id,email,domain,city\n1,ada,example.com,London\n2,plain,,Lyon\n";
        let source = fixture(source_bytes);
        #[cfg(unix)]
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        let original = session(&source, b',', HeaderMode::FirstRow);
        let working_path = destination(&source, "working.csv");
        let transformation = ColumnTransformation::split_with_blank_headers(
            1,
            b"@".to_vec(),
            2,
            Some(b"email".to_vec()),
        )
        .unwrap();
        let job = original
            .start_create_working_copy(
                BTreeMap::new(),
                BTreeMap::new(),
                transformation,
                &working_path,
            )
            .unwrap();

        wait_until_save_done(&job);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("working-copy save unexpectedly cancelled");
        };
        assert_eq!(summary.destination, working_path);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&working_path).unwrap(), materialized);
        assert!(temporary_exports(&working_path).is_empty());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&working_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let working = session(&working_path, b',', HeaderMode::FirstRow);
        let job = working
            .start_save_to_original(
                BTreeMap::from([(2, b"domain".to_vec())]),
                BTreeMap::from([((2, 3), b"Lyon".to_vec())]),
                &original,
            )
            .unwrap();
        wait_until_save_done(&job);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("save to original unexpectedly cancelled");
        };
        assert_eq!(summary.destination, source);
        assert_eq!(fs::read(&source).unwrap(), saved);
        assert_eq!(fs::read(&working_path).unwrap(), materialized);
        assert!(temporary_exports(&source).is_empty());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&source).unwrap().permissions().mode() & 0o777,
            0o640
        );

        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);
    }

    #[test]
    fn working_copy_pads_a_short_header_for_a_later_wider_split_column() {
        let source = fixture(b"id\n1,a@b\n");
        let original = session(&source, b',', HeaderMode::FirstRow);
        assert_eq!(original.first_rows[0].fields, [b"id".to_vec()]);
        assert_eq!(
            original.first_rows[1].fields,
            [b"1".to_vec(), b"a@b".to_vec()]
        );

        let working_path = destination(&source, "working.csv");
        let transformation =
            ColumnTransformation::split_with_blank_headers(1, b"@".to_vec(), 2, Some(Vec::new()))
                .unwrap();
        let job = original
            .start_create_working_copy(
                BTreeMap::new(),
                BTreeMap::new(),
                transformation,
                &working_path,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&working_path).unwrap(), b"id,,\n1,a,b\n");
        let reopened = session(&working_path, b',', HeaderMode::FirstRow);
        assert_eq!(
            reopened.first_rows[0].fields,
            [b"id".to_vec(), Vec::new(), Vec::new()]
        );
        assert_eq!(
            reopened.first_rows[1].fields,
            [b"1".to_vec(), b"a".to_vec(), b"b".to_vec()]
        );

        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_to_original_rejects_an_externally_changed_original() {
        let source = fixture(b"id,email\n1,ada@example.com\n");
        let original = session(&source, b',', HeaderMode::FirstRow);
        let working_path = destination(&source, "working.csv");
        fs::write(&working_path, b"id,email,\n1,ada,example.com\n").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&working_path, fs::Permissions::from_mode(0o600)).unwrap();
        let working = session(&working_path, b',', HeaderMode::FirstRow);
        let external = b"id,email\n1,externally changed\n";
        fs::write(&source, external).unwrap();

        assert!(matches!(
            working.start_save_to_original(BTreeMap::new(), BTreeMap::new(), &original),
            Err(QuarryError::SourceChanged)
        ));
        assert_eq!(fs::read(&source).unwrap(), external);
        assert!(temporary_exports(&source).is_empty());

        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_to_original_does_not_publish_when_original_changes_during_copy() {
        let source = fixture(b"id,email\n1,original@example.com\n");
        let original = session(&source, b',', HeaderMode::FirstRow);
        let working_path = destination(&source, "working.csv");
        let mut working_bytes = b"id,email,\n".to_vec();
        working_bytes.extend_from_slice(&b"1,local,example.com\n".repeat(100_000));
        fs::write(&working_path, &working_bytes).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&working_path, fs::Permissions::from_mode(0o600)).unwrap();
        let working = session(&working_path, b',', HeaderMode::FirstRow);
        let job = SaveAsJob::start_with_edits(
            working_path.clone(),
            working.file_size,
            b',',
            true,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: None,
            },
            SaveTarget::Existing {
                destination: source.clone(),
                source_expected: working.source_stamp.clone(),
                destination_expected: original.source_stamp.clone(),
            },
            ExportConfig {
                chunk_bytes: 64,
                max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while job.progress().bytes_scanned < 100 {
            assert!(!job.progress().done, "save completed before source change");
            assert!(Instant::now() < deadline, "save did not make progress");
            thread::yield_now();
        }

        let external = b"id,email\n1,external@example.com\n";
        fs::write(&source, external).unwrap();
        wait_until_save_done(&job);
        assert!(matches!(job.wait(), Err(QuarryError::SourceChanged)));
        assert_eq!(fs::read(&source).unwrap(), external);
        assert_eq!(fs::read(&working_path).unwrap(), working_bytes);
        assert!(temporary_exports(&source).is_empty());

        fs::remove_file(&source).unwrap();
        fs::remove_file(working_path).unwrap();
        remove_case(&source);
    }

    #[test]
    fn cancelling_a_private_working_copy_removes_its_owner_only_staging_file() {
        let source = fixture(&vec![b'a'; 4 * 1024 * 1024]);
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let working_path = destination(&source, "cancelled-working.csv");
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            source_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: Some(ColumnTransformation::Split {
                    source_column: 0,
                    separator: b"@".to_vec(),
                    output_count: 2,
                    output_headers: None,
                }),
            },
            SaveTarget::WorkingCopy(working_path.clone(), source_session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
            },
        )
        .unwrap();

        let temporary = temporary_exports(&working_path);
        assert_eq!(temporary.len(), 1);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&temporary[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
        job.cancel();
        wait_until_save_done(&job);
        assert_eq!(job.wait().unwrap(), SaveAsOutcome::Cancelled);
        assert!(!working_path.exists());
        assert!(temporary_exports(&working_path).is_empty());

        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn failed_private_working_copy_removes_staging_and_destination_files() {
        let source = fixture(b"id,email\n1,ada@example.com\n");
        let source_session = session(&source, b',', HeaderMode::FirstRow);
        let working_path = destination(&source, "failed-working.csv");
        let job = source_session
            .start_create_working_copy(
                BTreeMap::new(),
                BTreeMap::from([((1, 9), b"invalid".to_vec())]),
                ColumnTransformation::split_with_blank_headers(
                    1,
                    b"@".to_vec(),
                    2,
                    Some(b"email".to_vec()),
                )
                .unwrap(),
                &working_path,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait(), Err(QuarryError::InvalidOption(_))));
        assert!(!working_path.exists());
        assert!(temporary_exports(&working_path).is_empty());

        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_streams_ordered_join_and_fills_a_missing_selected_field() {
        let source_bytes = b"first,last,age,city\nAda,Lovelace,36,London\nGrace,,85";
        let expected = b"city and first,last,years\n\"Paris, Ada\",Lovelace,36\n\", Grace\",,85";
        let source = fixture(source_bytes);
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = session
            .start_save_with_transformation(
                BTreeMap::from([(2, b"years".to_vec())]),
                BTreeMap::from([((1, 3), b"Paris".to_vec())]),
                ColumnTransformation::Join {
                    source_columns: vec![3, 0],
                    separator: b", ".to_vec(),
                    output_header: Some(b"city and first".to_vec()),
                },
            )
            .unwrap();

        wait_until_save_done(&job);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("join save unexpectedly cancelled");
        };
        assert_eq!(summary.destination, source);
        assert_eq!(fs::read(&source).unwrap(), expected);
        assert!(temporary_exports(&source).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn transformations_reject_unsafe_shapes_and_oversized_output_without_publishing() {
        assert!(matches!(
            ColumnTransformation::Split {
                source_column: 0,
                separator: Vec::new(),
                output_count: 2,
                output_headers: None,
            }
            .transform_fields(&[b"value".to_vec()]),
            Err(QuarryError::InvalidOption(_))
        ));
        assert!(matches!(
            ColumnTransformation::Split {
                source_column: 0,
                separator: b":".to_vec(),
                output_count: MAX_TRANSFORMATION_COLUMNS + 1,
                output_headers: None,
            }
            .transform_fields(&[b"value".to_vec()]),
            Err(QuarryError::InvalidOption(_))
        ));
        assert!(matches!(
            ColumnTransformation::Join {
                source_columns: vec![0, 0],
                separator: Vec::new(),
                output_header: None,
            }
            .transform_fields(&[b"value".to_vec()]),
            Err(QuarryError::InvalidOption(_))
        ));
        assert_eq!(
            ColumnTransformation::Join {
                source_columns: vec![3, 0],
                separator: Vec::new(),
                output_header: Some(b"joined".to_vec()),
            }
            .transform_header_fields(&[b"first".to_vec(), b"last".to_vec()])
            .unwrap(),
            [b"joined".to_vec(), b"last".to_vec(), Vec::new()]
        );

        let source_bytes = b"a,b\n";
        let source = fixture(source_bytes);
        let destination = destination(&source, "oversized-transform.csv");
        let session = session(&source, b',', HeaderMode::NoHeader);
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: Some(ColumnTransformation::Join {
                    source_columns: vec![0, 1],
                    separator: b"too long".to_vec(),
                    output_header: None,
                }),
            },
            SaveTarget::New(destination.clone(), session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 2,
                max_record_bytes: 8,
            },
        )
        .unwrap();
        wait_until_save_done(&job);
        assert!(matches!(
            job.wait(),
            Err(QuarryError::RecordTooLarge { limit: 8 })
        ));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn oversized_save_inputs_are_rejected_before_worker_or_publication() {
        let source_bytes = b"a,b\nx,y\n";
        let source = fixture(source_bytes);
        let destination = destination(&source, "oversized-preflight.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let cases = vec![
            (
                "header edit",
                SaveEdits {
                    headers: BTreeMap::from([(0, vec![b'h'; 9])]),
                    cells: BTreeMap::new(),
                    transformation: None,
                },
            ),
            (
                "header edit aggregate",
                SaveEdits {
                    headers: BTreeMap::from([(0, vec![b'h'; 5]), (1, vec![b'i'; 4])]),
                    cells: BTreeMap::new(),
                    transformation: None,
                },
            ),
            (
                "cell edit",
                SaveEdits {
                    headers: BTreeMap::new(),
                    cells: BTreeMap::from([((1, 0), vec![b'c'; 9])]),
                    transformation: None,
                },
            ),
            (
                "cell edit aggregate",
                SaveEdits {
                    headers: BTreeMap::new(),
                    cells: BTreeMap::from([((1, 0), vec![b'c'; 5]), ((1, 1), vec![b'd'; 4])]),
                    transformation: None,
                },
            ),
            (
                "split output-header aggregate",
                SaveEdits {
                    headers: BTreeMap::new(),
                    cells: BTreeMap::new(),
                    transformation: Some(ColumnTransformation::Split {
                        source_column: 0,
                        separator: b":".to_vec(),
                        output_count: 2,
                        output_headers: Some(vec![b"first".to_vec(), b"last".to_vec()]),
                    }),
                },
            ),
            (
                "join separator",
                SaveEdits {
                    headers: BTreeMap::new(),
                    cells: BTreeMap::new(),
                    transformation: Some(ColumnTransformation::Join {
                        source_columns: vec![0, 1],
                        separator: vec![b'j'; 9],
                        output_header: Some(b"joined".to_vec()),
                    }),
                },
            ),
            (
                "join output header",
                SaveEdits {
                    headers: BTreeMap::new(),
                    cells: BTreeMap::new(),
                    transformation: Some(ColumnTransformation::Join {
                        source_columns: vec![0, 1],
                        separator: b" ".to_vec(),
                        output_header: Some(vec![b'o'; 9]),
                    }),
                },
            ),
        ];

        for (label, edits) in cases {
            let result = SaveAsJob::start_with_edits(
                source.clone(),
                session.file_size,
                b',',
                true,
                edits,
                SaveTarget::New(destination.clone(), session.source_stamp.clone()),
                ExportConfig {
                    chunk_bytes: 2,
                    max_record_bytes: 8,
                },
            );
            assert!(
                matches!(result, Err(QuarryError::RecordTooLarge { limit: 8 })),
                "{label} was not rejected during preflight"
            );
            assert!(!destination.exists(), "{label} published a destination");
            assert!(
                temporary_exports(&destination).is_empty(),
                "{label} left a temporary output"
            );
        }
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn preflight_allows_clone_bounds_that_serialize_within_the_limit() {
        let source = fixture(b"a,b");
        let joined_destination = destination(&source, "joined.csv");
        let headerless_session = session(&source, b',', HeaderMode::NoHeader);
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            headerless_session.file_size,
            b',',
            false,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::from([((0, 0), b"cccc".to_vec()), ((0, 1), b"dddd".to_vec())]),
                transformation: Some(ColumnTransformation::Join {
                    source_columns: vec![0, 1],
                    separator: Vec::new(),
                    output_header: None,
                }),
            },
            SaveTarget::New(
                joined_destination.clone(),
                headerless_session.source_stamp.clone(),
            ),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: 8,
            },
        )
        .unwrap();
        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&joined_destination).unwrap(), b"ccccdddd");
        fs::remove_file(&source).unwrap();
        fs::remove_file(joined_destination).unwrap();
        remove_case(&source);

        let source = fixture(b"a,b\nx,y");
        let destination = destination(&source, "split.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            session.file_size,
            b',',
            true,
            SaveEdits {
                headers: BTreeMap::from([(0, vec![b'i'; 9])]),
                cells: BTreeMap::new(),
                transformation: Some(ColumnTransformation::Split {
                    source_column: 0,
                    separator: b"separator longer than limit".to_vec(),
                    output_count: 2,
                    output_headers: Some(vec![b"l".to_vec(), b"r".to_vec()]),
                }),
            },
            SaveTarget::New(destination.clone(), session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: 8,
            },
        )
        .unwrap();
        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&destination).unwrap(), b"l,r,b\nx,,y");
        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn excessive_source_and_split_output_widths_do_not_publish() {
        let cases = [
            (
                MAX_TRANSFORMATION_COLUMNS + 1,
                "source-width.csv",
                ColumnTransformation::Join {
                    source_columns: vec![0, 1],
                    separator: Vec::new(),
                    output_header: None,
                },
            ),
            (
                MAX_TRANSFORMATION_COLUMNS,
                "output-width.csv",
                ColumnTransformation::Split {
                    source_column: 0,
                    separator: b":".to_vec(),
                    output_count: 2,
                    output_headers: None,
                },
            ),
        ];

        for (columns, name, transformation) in cases {
            let mut source_bytes = Vec::with_capacity(columns.saturating_mul(2));
            for column in 0..columns {
                if column > 0 {
                    source_bytes.push(b',');
                }
                source_bytes.push(b'x');
            }
            let source = fixture(&source_bytes);
            let destination = destination(&source, name);
            let session = session(&source, b',', HeaderMode::NoHeader);
            let job = session
                .start_save_as_with_transformation(
                    BTreeMap::new(),
                    BTreeMap::new(),
                    transformation,
                    &destination,
                )
                .unwrap();
            wait_until_save_done(&job);
            assert!(matches!(job.wait(), Err(QuarryError::InvalidOption(_))));
            assert!(!destination.exists());
            assert!(temporary_exports(&destination).is_empty());
            fs::remove_file(&source).unwrap();
            remove_case(&source);
        }
    }

    #[test]
    fn save_as_rewrites_only_the_header_and_preserves_source_and_data_bytes() {
        let source_bytes =
            b"\xEF\xBB\xBFdup;dup;\"old\nname\";empty\r\n1;\"raw\nrecord\";x;y\r\n2;\"\";a;b";
        let expected = b"\xEF\xBB\xBF\"renamed; \"\"quoted\"\"\nline\";dup;;dup\r\n1;\"raw\nrecord\";x;y\r\n2;\"\";a;b";
        let source = fixture(source_bytes);
        let destination = destination(&source, "renamed.csv");
        let session = session(&source, b';', HeaderMode::FirstRow);
        let renames = BTreeMap::from([
            (0, b"renamed; \"quoted\"\nline".to_vec()),
            (2, Vec::new()),
            (3, b"dup".to_vec()),
        ]);
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            session.file_size,
            b';',
            true,
            SaveEdits {
                headers: renames,
                cells: BTreeMap::new(),
                transformation: None,
            },
            SaveTarget::New(destination.clone(), session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 3,
                max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
            },
        )
        .unwrap();

        wait_until_save_done(&job);
        let progress = job.progress();
        assert_eq!(progress.bytes_scanned, source_bytes.len() as u64);
        assert_eq!(progress.bytes_written, expected.len() as u64);
        assert_eq!(progress.total_bytes, source_bytes.len() as u64);
        assert!(!progress.cancelled);
        assert!(job.error().is_none());

        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("save-as unexpectedly cancelled");
        };
        assert_eq!(summary.destination, destination);
        assert_eq!(summary.bytes_written, expected.len() as u64);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_as_streams_sparse_cell_edits_with_header_and_csv_fidelity() {
        let source_bytes = b"\xEF\xBB\xBFid;note;empty\r\n1;\"old\nline\";x\r\n2;\"say \"\"hi\"\"\";keep\n3;remove;tail";
        let expected = b"\xEF\xBB\xBFid;memo;empty\r\n\"1;changed\";\"new\nline\";\r\n2;\"say \"\"bye\"\"\";keep\n3;;tail";
        let source = fixture(source_bytes);
        let destination = destination(&source, "edited.csv");
        let session = session(&source, b';', HeaderMode::FirstRow);
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            session.file_size,
            b';',
            true,
            SaveEdits {
                headers: BTreeMap::from([(1, b"memo".to_vec())]),
                cells: BTreeMap::from([
                    ((1, 0), b"1;changed".to_vec()),
                    ((1, 1), b"new\nline".to_vec()),
                    ((1, 2), Vec::new()),
                    ((2, 1), b"say \"bye\"".to_vec()),
                    ((3, 1), Vec::new()),
                ]),
                transformation: None,
            },
            SaveTarget::New(destination.clone(), session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 2,
                max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
            },
        )
        .unwrap();

        wait_until_save_done(&job);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("save-as unexpectedly cancelled");
        };
        assert_eq!(summary.bytes_written, expected.len() as u64);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_as_edits_the_first_headerless_bom_record() {
        let source_bytes = b"\xEF\xBB\xBF1,old\r\n2,last";
        let expected = b"\xEF\xBB\xBF1,\"new\nline\"\r\n2,last";
        let source = fixture(source_bytes);
        let destination = destination(&source, "headerless.csv");
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let job = source_session
            .start_save_as_with_edits(
                BTreeMap::new(),
                BTreeMap::from([((0, 1), b"new\nline".to_vec())]),
                &destination,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn transformed_bom_output_with_a_quoted_first_field_reopens() {
        let source_bytes = b"\xEF\xBB\xBFone,two\n";
        let expected = b"\xEF\xBB\xBF\"two, one\"\n";
        let source = fixture(source_bytes);
        let destination = destination(&source, "bom-quoted.csv");
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let job = source_session
            .start_save_as_with_transformation(
                BTreeMap::new(),
                BTreeMap::new(),
                ColumnTransformation::Join {
                    source_columns: vec![1, 0],
                    separator: b", ".to_vec(),
                    output_header: None,
                },
                &destination,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&destination).unwrap(), expected);

        let reopened = session(&destination, b',', HeaderMode::NoHeader);
        assert_eq!(reopened.first_rows[0].fields, [b"two, one".to_vec()]);
        let index = reopened
            .start_indexing(IndexConfig {
                chunk_bytes: 1,
                checkpoint_every: 1,
                memory_budget_bytes: 64,
            })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            reopened.read_rows(&index, 0, 1).unwrap()[0].fields,
            [b"two, one".to_vec()]
        );

        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn transformed_leading_bom_data_stays_quoted_and_reopens_losslessly() {
        let source_bytes = b"\"\xEF\xBB\xBFone\",two\n";
        let expected = b"\"\xEF\xBB\xBFone|two\"\n";
        let source = fixture(source_bytes);
        let destination = destination(&source, "bom-data.csv");
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        assert_eq!(source_session.first_rows[0].fields[0], b"\xEF\xBB\xBFone");
        let job = source_session
            .start_save_as_with_transformation(
                BTreeMap::new(),
                BTreeMap::new(),
                ColumnTransformation::Join {
                    source_columns: vec![0, 1],
                    separator: b"|".to_vec(),
                    output_header: None,
                },
                &destination,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&destination).unwrap(), expected);
        let reopened = session(&destination, b',', HeaderMode::NoHeader);
        assert_eq!(
            reopened.first_rows[0].fields,
            [b"\xEF\xBB\xBFone|two".to_vec()]
        );

        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn transformed_unterminated_empty_record_stays_present() {
        let source = fixture(b",");
        let destination = destination(&source, "empty-record.csv");
        let source_session = session(&source, b',', HeaderMode::NoHeader);
        let job = source_session
            .start_save_as_with_transformation(
                BTreeMap::new(),
                BTreeMap::new(),
                ColumnTransformation::Join {
                    source_columns: vec![0, 1],
                    separator: Vec::new(),
                    output_header: None,
                },
                &destination,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&destination).unwrap(), b"\"\"");
        let reopened = session(&destination, b',', HeaderMode::NoHeader);
        assert_eq!(reopened.first_rows.len(), 1);
        assert_eq!(reopened.first_rows[0].fields, [Vec::<u8>::new()]);

        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn invalid_cell_coordinates_do_not_publish() {
        let source_bytes = b"id,name\n1,Ada\n";
        let source = fixture(source_bytes);
        let destination = destination(&source, "invalid-cell.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = session
            .start_save_as_with_edits(
                BTreeMap::new(),
                BTreeMap::from([((1, 2), b"extra".to_vec())]),
                &destination,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait(), Err(QuarryError::InvalidOption(_))));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        let job = session
            .start_save_as_with_edits(
                BTreeMap::new(),
                BTreeMap::from([((9, 0), b"missing".to_vec())]),
                &destination,
            )
            .unwrap();
        wait_until_save_done(&job);
        assert!(matches!(job.wait(), Err(QuarryError::InvalidOption(_))));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        assert!(matches!(
            session.start_save_with_edits(
                BTreeMap::new(),
                BTreeMap::from([((0, 0), b"header".to_vec())]),
            ),
            Err(QuarryError::InvalidOption(_))
        ));
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn serialized_record_limit_counts_the_edited_cell() {
        let source_bytes = b"id,name\n1,A\n";
        let source = fixture(source_bytes);
        let destination = destination(&source, "oversized-cell.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            session.file_size,
            b',',
            true,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::from([((1, 1), b"too long".to_vec())]),
                transformation: None,
            },
            SaveTarget::New(destination.clone(), session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 3,
                max_record_bytes: 8,
            },
        )
        .unwrap();
        wait_until_save_done(&job);
        assert!(matches!(
            job.wait(),
            Err(QuarryError::RecordTooLarge { limit: 8 })
        ));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn cell_save_streams_unedited_records_larger_than_the_edit_buffer() {
        let source_bytes = b"id,name\n1,an untouched long value\n2,A\n";
        let expected = b"id,name\n1,an untouched long value\n2,B\n";
        let source = fixture(source_bytes);
        let destination = destination(&source, "bounded-cell.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            session.file_size,
            b',',
            true,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::from([((2, 1), b"B".to_vec())]),
                transformation: None,
            },
            SaveTarget::New(destination.clone(), session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 3,
                max_record_bytes: 8,
            },
        )
        .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(job.wait().unwrap(), SaveAsOutcome::Complete(_)));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_replaces_the_source_with_edits_and_preserves_permissions() {
        let source = fixture(b"id,name\n1,Ada\n");
        #[cfg(unix)]
        {
            fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
            let metadata = File::open(&source).unwrap().metadata().unwrap();
            let stamp = crate::SourceStamp::from_metadata(&metadata);
            let output = ExportTarget::replace_source(&source, metadata, stamp).unwrap();
            assert_eq!(
                fs::metadata(&output.temporary)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
            output.discard().unwrap();
        }
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = session
            .start_save_with_edits(
                BTreeMap::from([(1, b"person".to_vec())]),
                BTreeMap::from([((1, 1), b"Grace".to_vec())]),
            )
            .unwrap();

        wait_until_save_done(&job);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("save unexpectedly cancelled");
        };
        assert_eq!(summary.destination, source);
        assert_eq!(fs::read(&source).unwrap(), b"id,person\n1,Grace\n");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&source).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(temporary_exports(&source).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn failed_save_preserves_the_source_and_removes_its_temporary_file() {
        let source_bytes = b"id,name\n1,Ada\n";
        let source = fixture(source_bytes);
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = session
            .start_save_with_header_renames(BTreeMap::from([(2, b"missing".to_vec())]))
            .unwrap();

        wait_until_save_done(&job);
        assert!(matches!(
            job.wait(),
            Err(QuarryError::InvalidOption(
                "header rename column is out of range"
            ))
        ));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(temporary_exports(&source).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn cancelled_save_before_publication_preserves_the_source() {
        let source = fixture(b"original");
        let metadata = File::open(&source).unwrap().metadata().unwrap();
        let stamp = crate::SourceStamp::from_metadata(&metadata);
        let mut output = ExportTarget::replace_source(&source, metadata, stamp).unwrap();
        assert_eq!(output.temporary.parent(), source.parent());
        output.write_all(b"replacement").unwrap();

        assert_eq!(
            output.publish(0, 11, &AtomicBool::new(true)).unwrap(),
            FilterExportOutcome::Cancelled
        );
        assert_eq!(fs::read(&source).unwrap(), b"original");
        assert!(temporary_exports(&source).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_does_not_overwrite_an_external_source_change() {
        let source = fixture(b"original");
        let metadata = File::open(&source).unwrap().metadata().unwrap();
        let stamp = crate::SourceStamp::from_metadata(&metadata);
        let mut output = ExportTarget::replace_source(&source, metadata, stamp).unwrap();
        output.write_all(b"quarry replacement").unwrap();
        fs::write(&source, b"external replacement").unwrap();

        assert!(matches!(
            output.publish(0, 18, &AtomicBool::new(false)),
            Err(QuarryError::SourceChanged)
        ));
        assert_eq!(fs::read(&source).unwrap(), b"external replacement");
        assert!(temporary_exports(&source).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn session_source_guard_accepts_the_opened_file_and_rejects_a_replacement() {
        let source = fixture(b"id\n1\n");
        let session = session(&source, b',', HeaderMode::FirstRow);
        assert!(session.ensure_source_unchanged().is_ok());

        let replacement = destination(&source, "replacement.csv");
        fs::write(&replacement, b"externally replaced\n").unwrap();
        fs::remove_file(&source).unwrap();
        fs::rename(&replacement, &source).unwrap();

        assert!(matches!(
            session.ensure_source_unchanged(),
            Err(QuarryError::SourceChanged)
        ));
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_rejects_a_source_changed_since_the_session_opened() {
        let source = fixture(b"id,name\n1,Ada\n");
        let destination = destination(&source, "stale-save-as.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        fs::write(&source, b"id,name\n1,Grace\n").unwrap();

        assert!(matches!(
            session.start_save_with_header_renames(BTreeMap::from([(0, b"ID".to_vec())])),
            Err(QuarryError::SourceChanged)
        ));
        assert!(matches!(
            session.start_save_as_with_header_renames(
                BTreeMap::from([(0, b"ID".to_vec())]),
                &destination,
            ),
            Err(QuarryError::SourceChanged)
        ));
        assert_eq!(fs::read(&source).unwrap(), b"id,name\n1,Grace\n");
        assert!(!destination.exists());
        assert!(temporary_exports(&source).is_empty());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_as_reports_source_changed_when_the_opened_source_was_renamed() {
        let source_bytes = b"id,name\n1,Ada\n";
        let source = fixture(source_bytes);
        let moved_source = destination(&source, "moved-source.csv");
        let destination = destination(&source, "renamed-source-save.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        fs::rename(&source, &moved_source).unwrap();

        assert!(matches!(
            session.start_save_as_with_header_renames(
                BTreeMap::from([(0, b"ID".to_vec())]),
                &destination,
            ),
            Err(QuarryError::SourceChanged)
        ));
        assert_eq!(fs::read(&moved_source).unwrap(), source_bytes);
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&moved_source).unwrap();
        remove_case(&source);
    }

    #[cfg(unix)]
    #[test]
    fn save_rejects_a_symbolic_link_without_changing_it_or_its_target() {
        let source = fixture(b"id,name\n1,Ada\n");
        let link = destination(&source, "linked.csv");
        symlink(&source, &link).unwrap();
        let session = session(&link, b',', HeaderMode::FirstRow);

        assert!(matches!(
            session.start_save_with_header_renames(BTreeMap::from([(0, b"ID".to_vec())])),
            Err(QuarryError::InvalidOption(
                "saving through a symbolic link is not supported; use Save As instead"
            ))
        ));
        assert_eq!(fs::read(&source).unwrap(), b"id,name\n1,Ada\n");
        assert_eq!(fs::read_link(&link).unwrap(), source);
        assert!(temporary_exports(&link).is_empty());
        fs::remove_file(link).unwrap();
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_as_rejects_unsafe_destinations_and_headerless_sources() {
        let source = fixture(b"id,name\n1,Ada\n");
        let existing = destination(&source, "existing.csv");
        fs::write(&existing, b"existing").unwrap();
        let with_header = session(&source, b',', HeaderMode::FirstRow);
        let without_header = session(&source, b',', HeaderMode::NoHeader);
        let renames = BTreeMap::from([(0, b"ID".to_vec())]);

        assert!(matches!(
            with_header.start_save_as_with_header_renames(renames.clone(), &source),
            Err(QuarryError::ExportDestinationIsSource)
        ));
        assert!(matches!(
            with_header.start_save_as_with_header_renames(renames.clone(), &existing),
            Err(QuarryError::ExportDestinationExists)
        ));
        assert!(matches!(
            without_header
                .start_save_as_with_header_renames(renames, destination(&source, "no-header.csv")),
            Err(QuarryError::InvalidOption(
                "header renames require a header row"
            ))
        ));
        assert_eq!(fs::read(&source).unwrap(), b"id,name\n1,Ada\n");
        assert_eq!(fs::read(&existing).unwrap(), b"existing");
        assert!(temporary_exports(&existing).is_empty());
        fs::remove_file(&source).unwrap();
        fs::remove_file(existing).unwrap();
        remove_case(&source);
    }

    #[test]
    fn invalid_header_rename_cleans_up_without_publishing() {
        let source = fixture(b"id,name\n1,Ada\n");
        let destination = destination(&source, "invalid.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = session
            .start_save_as_with_header_renames(
                BTreeMap::from([(2, b"missing".to_vec())]),
                &destination,
            )
            .unwrap();

        wait_until_save_done(&job);
        assert!(job.error().unwrap().contains("out of range"));
        assert!(matches!(
            job.wait(),
            Err(QuarryError::InvalidOption(
                "header rename column is out of range"
            ))
        ));
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn serialized_header_limit_counts_the_renamed_value() {
        let source_bytes = b"id,name\n1,Ada\n";
        let source = fixture(source_bytes);
        let destination = destination(&source, "oversized-header.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let result = SaveAsJob::start_with_edits(
            source.clone(),
            session.file_size,
            b',',
            true,
            SaveEdits {
                headers: BTreeMap::from([(0, b"a renamed header".to_vec())]),
                cells: BTreeMap::new(),
                transformation: None,
            },
            SaveTarget::New(destination.clone(), session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 4,
                max_record_bytes: 8,
            },
        );
        assert!(matches!(
            result,
            Err(QuarryError::RecordTooLarge { limit: 8 })
        ));
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn cancelling_transformed_save_as_removes_partial_output() {
        let mut source_bytes = b"id,name\n".to_vec();
        source_bytes.extend_from_slice(&b"1,Ada\n".repeat(500_000));
        let source = fixture(&source_bytes);
        let destination = destination(&source, "cancelled-save.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            session.file_size,
            b',',
            true,
            SaveEdits {
                headers: BTreeMap::new(),
                cells: BTreeMap::new(),
                transformation: Some(ColumnTransformation::Join {
                    source_columns: vec![0, 1],
                    separator: b" ".to_vec(),
                    output_header: Some(b"identity".to_vec()),
                }),
            },
            SaveTarget::New(destination.clone(), session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while job.progress().bytes_scanned < 100 {
            assert!(
                !job.progress().done,
                "save-as completed before cancellation"
            );
            assert!(Instant::now() < deadline, "save-as did not make progress");
            thread::yield_now();
        }
        assert_eq!(temporary_exports(&destination).len(), 1);

        job.cancel();
        wait_until_save_done(&job);
        assert!(job.progress().cancelled);
        assert_eq!(job.wait().unwrap(), SaveAsOutcome::Cancelled);
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn save_as_does_not_publish_when_source_changes_during_copy() {
        let mut source_bytes = b"id,name\n".to_vec();
        source_bytes.extend_from_slice(&b"1,Ada\n".repeat(500_000));
        let source = fixture(&source_bytes);
        let destination = destination(&source, "source-changed-save.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = SaveAsJob::start_with_edits(
            source.clone(),
            session.file_size,
            b',',
            true,
            SaveEdits {
                headers: BTreeMap::from([(0, b"ID".to_vec())]),
                cells: BTreeMap::new(),
                transformation: None,
            },
            SaveTarget::New(destination.clone(), session.source_stamp.clone()),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while job.progress().bytes_scanned < 100 {
            assert!(
                !job.progress().done,
                "save-as completed before source change"
            );
            assert!(Instant::now() < deadline, "save-as did not make progress");
            thread::yield_now();
        }

        let external = b"id,name\nexternal,change\n";
        fs::write(&source, external).unwrap();
        wait_until_save_done(&job);
        assert!(matches!(job.wait(), Err(QuarryError::SourceChanged)));
        assert_eq!(fs::read(&source).unwrap(), external);
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn exports_raw_header_and_matching_records_without_changing_the_source() {
        let source_bytes = b"id;note;kind\r\n1;\"line one\nline \"\"two\"\"\";keep\r\n2;plain;drop\r\n3;\"last;value\";KEEP";
        let expected =
            b"id;note;kind\r\n1;\"line one\nline \"\"two\"\"\";keep\r\n3;\"last;value\";KEEP";
        let source = fixture(source_bytes);
        let destination = destination(&source, "filtered.csv");
        let session = session(&source, b';', HeaderMode::FirstRow);
        let job = session
            .start_filtered_export(query(), &destination)
            .unwrap();

        wait_until_done(&job);
        let progress = job.progress();
        assert_eq!(progress.bytes_scanned, source_bytes.len() as u64);
        assert_eq!(progress.rows_scanned, 4);
        assert_eq!(progress.rows_written, 2);
        assert_eq!(progress.bytes_written, expected.len() as u64);
        assert_eq!(progress.total_bytes, source_bytes.len() as u64);
        assert!(!progress.cancelled);
        assert!(job.error().is_none());

        let FilterExportOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("export unexpectedly cancelled");
        };
        assert_eq!(summary.destination, destination);
        assert_eq!(summary.rows_written, 2);
        assert_eq!(summary.bytes_written, expected.len() as u64);
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn rejects_source_and_existing_destinations_without_overwriting() {
        let source = fixture(b"id,note,kind\n1,value,keep\n");
        let destination = destination(&source, "existing.csv");
        fs::write(&destination, b"existing").unwrap();
        let session = session(&source, b',', HeaderMode::FirstRow);

        assert!(matches!(
            session.start_filtered_export(query(), &source),
            Err(QuarryError::ExportDestinationIsSource)
        ));
        assert!(matches!(
            session.start_filtered_export(query(), &destination),
            Err(QuarryError::ExportDestinationExists)
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"existing");
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn rejects_the_same_normalized_path_after_the_source_link_disappears() {
        let source = fixture(b"source");
        let open_source = File::open(&source).unwrap();
        let destination = source.parent().unwrap().join(".").join("source.csv");
        fs::remove_file(&source).unwrap();

        assert!(matches!(
            ExportTarget::new(&source, destination.clone()),
            Err(QuarryError::ExportDestinationIsSource)
        ));
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        drop(open_source);
        remove_case(&source);
    }

    #[test]
    fn long_destination_names_use_a_short_temporary_name() {
        let source = fixture(b"source");
        let destination = destination(&source, &"x".repeat(240));
        let output = ExportTarget::new(&source, destination.clone()).unwrap();

        assert!(output.temporary.file_name().unwrap().len() < 64);
        output.discard().unwrap();
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn publication_does_not_clobber_a_destination_created_during_export() {
        let source = fixture(b"source");
        let destination = destination(&source, "raced.csv");
        let mut output = ExportTarget::new(&source, destination.clone()).unwrap();
        output.write_all(b"new").unwrap();
        fs::write(&destination, b"existing").unwrap();

        assert!(matches!(
            output.publish(1, 3, &AtomicBool::new(false)),
            Err(QuarryError::ExportDestinationExists)
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"existing");
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        fs::remove_file(destination).unwrap();
        remove_case(&source);
    }

    #[test]
    fn cancellation_before_publication_leaves_no_destination() {
        let source = fixture(b"source");
        let destination = destination(&source, "cancelled-at-publish.csv");
        let mut output = ExportTarget::new(&source, destination.clone()).unwrap();
        output.write_all(b"complete bytes").unwrap();
        let cancel_requested = AtomicBool::new(true);

        assert_eq!(
            output.publish(1, 14, &cancel_requested).unwrap(),
            FilterExportOutcome::Cancelled
        );
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn cancellation_before_publication_does_not_flush_buffered_output() {
        let source = fixture(b"source");
        let destination = destination(&source, "cancelled-buffered-publish.csv");
        let mut output = ExportTarget::new(&source, destination.clone()).unwrap();
        let mut unflushable = std::io::BufWriter::new(File::open(&output.temporary).unwrap());
        unflushable.write_all(b"buffered bytes").unwrap();
        drop(output.writer.replace(unflushable));

        assert_eq!(
            output.publish(1, 14, &AtomicBool::new(true)).unwrap(),
            FilterExportOutcome::Cancelled
        );
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn cancellation_removes_the_temporary_output() {
        let source = fixture(&b"miss,value,drop\n".repeat(500_000));
        let destination = destination(&source, "cancelled.csv");
        let session = session(&source, b',', HeaderMode::NoHeader);
        let job = FilterExportJob::start(
            source.clone(),
            session.file_size,
            b',',
            false,
            query(),
            destination.clone(),
            ExportConfig {
                chunk_bytes: 1,
                max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while job.progress().bytes_scanned < 100 {
            assert!(!job.progress().done, "export completed before cancellation");
            assert!(Instant::now() < deadline, "export did not make progress");
            thread::yield_now();
        }
        assert_eq!(temporary_exports(&destination).len(), 1);

        job.cancel();
        wait_until_done(&job);
        assert!(job.progress().cancelled);
        assert_eq!(job.wait().unwrap(), FilterExportOutcome::Cancelled);
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn record_limit_errors_remove_the_temporary_output() {
        let source = fixture(b"123456789,tail,keep\n");
        let destination = destination(&source, "failed.csv");
        let session = session(&source, b',', HeaderMode::NoHeader);
        let job = FilterExportJob::start(
            source.clone(),
            session.file_size,
            b',',
            false,
            query(),
            destination.clone(),
            ExportConfig {
                chunk_bytes: 4,
                max_record_bytes: 8,
            },
        )
        .unwrap();

        wait_until_done(&job);
        assert!(job.error().unwrap().contains("8-byte limit"));
        assert!(matches!(
            job.wait(),
            Err(QuarryError::RecordTooLarge { limit: 8 })
        ));
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }

    #[test]
    fn dropping_an_active_export_cancels_joins_and_removes_its_temp_file() {
        let source = fixture(b"source");
        let destination = destination(&source, "dropped.csv");
        let output = ExportTarget::new(&source, destination.clone()).unwrap();
        assert_eq!(temporary_exports(&destination).len(), 1);
        let shared = Arc::new(SharedState::new(100));
        let worker_state = Arc::clone(&shared);
        let handle = thread::spawn(move || {
            let _completion = WorkerCompletion(&worker_state);
            let deadline = Instant::now() + Duration::from_secs(2);
            while !worker_state.cancel_requested.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "export was not cancelled");
                thread::yield_now();
            }
            output.discard()?;
            worker_state.cancelled.store(true, Ordering::Release);
            Ok(FilterExportOutcome::Cancelled)
        });
        let job = FilterExportJob {
            shared: Arc::clone(&shared),
            handle: Some(handle),
        };

        drop(job);

        assert!(shared.cancel_requested.load(Ordering::Acquire));
        assert!(shared.cancelled.load(Ordering::Acquire));
        assert!(shared.done.load(Ordering::Acquire));
        assert!(!destination.exists());
        assert!(temporary_exports(&destination).is_empty());
        fs::remove_file(&source).unwrap();
        remove_case(&source);
    }
}
