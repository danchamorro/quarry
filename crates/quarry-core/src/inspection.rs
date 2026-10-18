//! Bounded, source-preserving inspection and explicitly selected recovery copies.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};

use crate::export::{ExportTarget, source_matches_stamp};
use crate::{Encoding, HeaderMode, QuarryError, RecordScanner, Row, SourceStamp};
use quarry_delimited::ParseErrorKind;

const FINDING_LIMIT: usize = 1000;
const PREVIEW_LIMIT: usize = 8192;
const FIELD_LIMIT: usize = crate::MAX_TRANSFORMATION_COLUMNS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    Structure,
    Encoding,
    RowWidth,
    Limit,
    Operation,
}
impl DiagnosticKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Structure => "Record structure",
            Self::Encoding => "Text encoding",
            Self::RowWidth => "Uneven row",
            Self::Limit => "Resource limit",
            Self::Operation => "Operation",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
    pub advice: String,
    /// One-based logical record, including the header if present.
    pub record: Option<u64>,
    pub data_row: Option<u64>,
    pub column: Option<usize>,
    pub physical_line: Option<u64>,
    /// Zero-based offset in the encoded source, including its BOM.
    pub byte_offset: Option<u64>,
    pub record_range: Option<Range<u64>>,
    pub edited: bool,
    pub blocking: bool,
}
impl Diagnostic {
    pub fn from_error(error: &QuarryError) -> Self {
        let mut d = Self::operation(error.to_string());
        match error {
            QuarryError::InvalidNumericSortValue { data_row, column }
            | QuarryError::InvalidCleanupText { data_row, column }
            | QuarryError::InvalidUtf8SortValue { data_row, column }
            | QuarryError::InvalidDateTimeSortValue {
                data_row, column, ..
            } => {
                d.data_row = Some(*data_row);
                d.column = Some(*column);
            }
            QuarryError::CombineFile { record, .. } => d.record = *record,
            QuarryError::RecordEncoding { record, .. } => {
                d.kind = DiagnosticKind::Encoding;
                d.record = Some(*record);
            }
            QuarryError::Encoding(_) | QuarryError::EncodingAt { .. } => {
                d.kind = DiagnosticKind::Encoding
            }
            QuarryError::RecordTooLarge { .. } | QuarryError::BootstrapLimitExceeded { .. } => {
                d.kind = DiagnosticKind::Limit
            }
            QuarryError::Parse(_) => d.kind = DiagnosticKind::Structure,
            _ => {}
        }
        d
    }
    pub fn operation(message: impl Into<String>) -> Self {
        Self { kind: DiagnosticKind::Operation, message: message.into(),
            advice:"The current document is preserved. Correct the cause and retry; Inspect File can locate source-data problems.".into(),
            record:None, data_row:None, column:None, physical_line:None, byte_offset:None,
            record_range:None, edited:false, blocking:true }
    }
    pub fn details(&self) -> String {
        let mut s = format!("{}\n{}\n", self.kind.label(), self.message);
        if let Some(v) = self.record {
            s += &format!("Record: {v}\n");
        }
        if let Some(v) = self.data_row {
            s += &format!("Data row: {v}\n");
        }
        if let Some(v) = self.column {
            s += &format!("Column: {v}\n");
        }
        if let Some(v) = self.physical_line {
            s += &format!("Physical line: {v}\n");
        }
        if let Some(v) = self.byte_offset {
            s += &format!("Source byte offset (zero-based): {v}\n");
        }
        if self.edited {
            s += "Location refers to an unsaved value; there is no original byte position.\n";
        }
        if self.record.is_none() && self.data_row.is_none() && self.column.is_none() {
            s += "Record and column: unavailable for this operation.\n";
        }
        if self.byte_offset.is_none() {
            s += "Original-file byte position: unavailable.\n";
        }
        s += &self.advice;
        s
    }
}

#[derive(Clone, Copy, Debug)]
pub struct InspectionOptions {
    pub delimiter: Option<u8>,
    pub header_mode: HeaderMode,
    pub encoding: Option<Encoding>,
    pub chunk_bytes: usize,
    pub max_record_bytes: usize,
}
impl Default for InspectionOptions {
    fn default() -> Self {
        Self {
            delimiter: None,
            header_mode: HeaderMode::Auto,
            encoding: None,
            chunk_bytes: crate::DEFAULT_READ_CHUNK,
            max_record_bytes: crate::DEFAULT_MAX_RECORD_BYTES,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InspectionEdits {
    pub headers: BTreeMap<usize, Vec<u8>>,
    /// Zero-based physical-record/column keys, identical to the editing APIs.
    pub cells: BTreeMap<(u64, usize), Vec<u8>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectionStatus {
    Complete,
    Stopped,
    Cancelled,
}
#[derive(Clone, Copy, Debug)]
pub struct InspectionProgress {
    pub bytes_scanned: u64,
    pub total_bytes: u64,
    pub records: u64,
    pub done: bool,
    pub phase: &'static str,
}
struct Progress {
    bytes: AtomicU64,
    total: AtomicU64,
    records: AtomicU64,
    cancel: AtomicBool,
    done: AtomicBool,
    phase: AtomicU8,
}
impl Progress {
    fn new() -> Self {
        Self {
            bytes: 0.into(),
            total: 0.into(),
            records: 0.into(),
            cancel: false.into(),
            done: false.into(),
            phase: 0.into(),
        }
    }
    fn snapshot(&self) -> InspectionProgress {
        InspectionProgress {
            bytes_scanned: self.bytes.load(Ordering::Acquire),
            total_bytes: self.total.load(Ordering::Acquire),
            records: self.records.load(Ordering::Acquire),
            done: self.done.load(Ordering::Acquire),
            phase: match self.phase.load(Ordering::Acquire) {
                1 => "Creating recovery copy",
                2 => "Validating recovery copy",
                _ => "Inspecting file",
            },
        }
    }
}
#[derive(Clone, Debug)]
pub struct SourcePreview {
    pub start: u64,
    pub end: u64,
    pub text: String,
}

impl SourcePreview {
    /// A bounded prefix for a failed open; it is not represented as an error location.
    pub fn prefix(path: &Path, requested: Option<Encoding>) -> Result<Self, QuarryError> {
        let mut f = File::open(path)?;
        let stamp = SourceStamp::from_file(&f)?;
        let encoding = Encoding::detect(&mut f, requested).unwrap_or(requested.unwrap_or_default());
        f.seek(SeekFrom::Start(0))?;
        let mut bytes = vec![0; PREVIEW_LIMIT];
        let n = f.read(&mut bytes)?;
        bytes.truncate(n);
        if !source_matches_stamp(&f, path, &stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        Ok(Self {
            start: 0,
            end: n as u64,
            text: escape_bytes(&bytes, encoding, None),
        })
    }
}

#[derive(Clone, Debug)]
pub struct InspectionReport {
    pub path: PathBuf,
    pub encoding: Encoding,
    pub delimiter: u8,
    pub has_header: bool,
    pub findings: Vec<Diagnostic>,
    pub total_findings: u64,
    pub blocking_findings: u64,
    pub records: u64,
    pub bytes_scanned: u64,
    pub total_bytes: u64,
    pub status: InspectionStatus,
    pub header_certain: bool,
    stamp: SourceStamp,
    options: InspectionOptions,
    edits: InspectionEdits,
}
impl InspectionReport {
    pub fn is_current(&self) -> bool {
        File::open(&self.path)
            .ok()
            .and_then(|f| source_matches_stamp(&f, &self.path, &self.stamp).ok())
            .unwrap_or(false)
    }
    pub fn matches_edits(&self, edits: &InspectionEdits) -> bool {
        self.edits == *edits
    }
    pub fn can_omit(&self, index: usize) -> bool {
        self.status == InspectionStatus::Complete
            && self.header_certain
            && self.findings.get(index).is_some_and(|d| {
                d.record_range.is_some() && d.record.is_some_and(|r| r > u64::from(self.has_header))
            })
    }
    pub fn selected_records(
        &self,
        selection: &BTreeSet<usize>,
    ) -> Result<BTreeSet<u64>, QuarryError> {
        if selection.is_empty() {
            return Err(QuarryError::InvalidOption(
                "select at least one complete data record",
            ));
        }
        let mut records = BTreeSet::new();
        for &i in selection {
            if !self.can_omit(i) {
                return Err(QuarryError::InvalidOption(
                    "recovery requires a completed inspection and completely identified data records; headers cannot be removed",
                ));
            }
            records.insert(
                self.findings[i]
                    .record
                    .expect("eligible finding has a record"),
            );
        }
        Ok(records)
    }
    pub fn preview(&self, index: usize) -> Result<SourcePreview, QuarryError> {
        let d = self
            .findings
            .get(index)
            .ok_or(QuarryError::InvalidOption("finding not found"))?;
        if !self.is_current() {
            return Err(QuarryError::SourceChanged);
        }
        if d.edited {
            let value = d.record.zip(d.column).and_then(|(r, c)| {
                if self.has_header && r == 1 {
                    self.edits.headers.get(&(c - 1))
                } else {
                    self.edits.cells.get(&(r - 1, c - 1))
                }
            });
            let bytes = value.map_or(&[][..], Vec::as_slice);
            return Ok(SourcePreview {
                start: 0,
                end: bytes.len().min(PREVIEW_LIMIT) as u64,
                text: format!(
                    "Unsaved value (source byte position unavailable)\n{}",
                    escape_bytes(
                        &bytes[..bytes.len().min(PREVIEW_LIMIT)],
                        Encoding::Utf8,
                        None
                    )
                ),
            });
        }
        let offset = d
            .byte_offset
            .or_else(|| d.record_range.as_ref().map(|r| r.start))
            .unwrap_or(0);
        let width = if self.encoding.is_utf16() { 2 } else { 1 };
        let mut start = offset.saturating_sub((PREVIEW_LIMIT / 2) as u64);
        start -= start % width;
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; PREVIEW_LIMIT];
        let n = file.read(&mut bytes)?;
        bytes.truncate(n);
        if !source_matches_stamp(&file, &self.path, &self.stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        Ok(SourcePreview {
            start,
            end: start + n as u64,
            text: escape_bytes(
                &bytes,
                self.encoding,
                Some(offset.saturating_sub(start) as usize),
            ),
        })
    }
    fn add(&mut self, diagnostic: Diagnostic) {
        self.total_findings = self.total_findings.saturating_add(1);
        self.blocking_findings += u64::from(diagnostic.blocking);
        if self.findings.len() < FINDING_LIMIT {
            self.findings.push(diagnostic);
        }
    }
}

pub struct InspectionJob {
    shared: Arc<Progress>,
    handle: Option<JoinHandle<Result<InspectionReport, QuarryError>>>,
}
impl InspectionJob {
    pub fn start(
        path: impl AsRef<Path>,
        options: InspectionOptions,
        edits: InspectionEdits,
    ) -> Result<Self, QuarryError> {
        validate_options(options)?;
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let stamp = SourceStamp::from_file(&file)?;
        let shared = Arc::new(Progress::new());
        shared.total.store(stamp.file_size(), Ordering::Release);
        let worker = Arc::clone(&shared);
        let handle = thread::spawn(move || {
            let result = scan(
                &mut file,
                &path,
                stamp,
                options,
                edits,
                &worker,
                &BTreeSet::new(),
                &mut |_, _, _| Ok(()),
            );
            worker.done.store(true, Ordering::Release);
            result
        });
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }
    pub fn progress(&self) -> InspectionProgress {
        self.shared.snapshot()
    }
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Release);
    }
    pub fn wait(mut self) -> Result<InspectionReport, QuarryError> {
        self.handle
            .take()
            .expect("inspection handle")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}
impl Drop for InspectionJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn validate_options(o: InspectionOptions) -> Result<(), QuarryError> {
    if o.chunk_bytes == 0
        || o.chunk_bytes > 8 * 1024 * 1024
        || o.max_record_bytes == 0
        || o.max_record_bytes > crate::DEFAULT_MAX_RECORD_BYTES
    {
        return Err(QuarryError::InvalidOption(
            "inspection needs a 1-byte to 8-MiB chunk and a record limit within the existing 64-MiB cap",
        ));
    }
    Ok(())
}
type RecordSink<'a> = dyn FnMut(u64, &[u8], Option<&[u8]>) -> Result<(), QuarryError> + 'a;

#[allow(clippy::too_many_arguments)]
fn scan(
    file: &mut File,
    path: &Path,
    stamp: SourceStamp,
    options: InspectionOptions,
    edits: InspectionEdits,
    progress: &Progress,
    omitted: &BTreeSet<u64>,
    sink: &mut RecordSink<'_>,
) -> Result<InspectionReport, QuarryError> {
    let total = stamp.file_size();
    let mut report = InspectionReport {
        path: path.to_owned(),
        encoding: options.encoding.unwrap_or_default(),
        delimiter: options.delimiter.unwrap_or(b','),
        has_header: options.header_mode == HeaderMode::FirstRow,
        header_certain: options.header_mode != HeaderMode::Auto,
        findings: Vec::new(),
        total_findings: 0,
        blocking_findings: 0,
        records: 0,
        bytes_scanned: 0,
        total_bytes: total,
        status: InspectionStatus::Complete,
        stamp,
        options,
        edits,
    };
    let encoding = match Encoding::detect(file, options.encoding) {
        Ok(e) => e,
        Err(e) => {
            report.add(error_diagnostic(&e, None, None, 0, 1, None));
            report.status = InspectionStatus::Stopped;
            return Ok(report);
        }
    };
    report.encoding = encoding;
    if options.delimiter.is_none() {
        // Reuse normal dialect selection. A malformed sample can still be inspected
        // with comma selected; the report explicitly carries that format.
        report.delimiter = crate::detect_file_delimiter(
            file,
            total,
            crate::DEFAULT_SAMPLE_BYTES,
            crate::DEFAULT_BOOTSTRAP_LIMIT,
            encoding,
        )
        .unwrap_or(b',');
    }
    let delimiter = report.delimiter;
    let mut scanner = RecordScanner::new_for_inspection(delimiter, encoding)?;
    file.seek(SeekFrom::Start(0))?;
    let mut chunk = vec![0; encoding.chunk_size(options.chunk_bytes)];
    let mut raw = Vec::new();
    let mut start = 0u64;
    let mut absolute = 0u64;
    let mut line = 1u64;
    let mut record_lfs = 0u64;
    let mut first_rows = Vec::new();
    let mut expected_width = None;
    loop {
        if progress.cancel.load(Ordering::Acquire) {
            report.status = InspectionStatus::Cancelled;
            break;
        }
        let mut read_error = None;
        let count = match encoding.read_chunk(file, &mut chunk) {
            Ok(n) => n,
            Err(e @ QuarryError::EncodingAt { .. }) => {
                // An odd EOF byte must not discard valid preceding records from
                // the same read. Frame the complete code units before reporting it.
                let end = file.stream_position()?;
                let valid = ((end.saturating_sub(absolute) as usize).min(chunk.len())) & !1;
                file.seek(SeekFrom::Start(absolute))?;
                file.read_exact(&mut chunk[..valid])?;
                read_error = Some(e);
                valid
            }
            Err(e) => {
                report.add(error_diagnostic(
                    &e,
                    Some(report.records + 1),
                    None,
                    start,
                    line,
                    None,
                ));
                report.status = InspectionStatus::Stopped;
                break;
            }
        };
        let mut ends = Vec::new();
        let mut result = if count == 0 && read_error.is_some() {
            Ok(())
        } else if count == 0 {
            scanner.finish(absolute, |end| ends.push(end)).map(|_| ())
        } else {
            scanner
                .scan_chunk(&chunk[..count], absolute, |end| ends.push(end))
                .map(|_| ())
        };
        if result.is_ok()
            && let Some(error) = read_error
        {
            result = Err(error);
        }
        let mut segment = 0usize;
        for end in ends {
            let n = (end - absolute) as usize;
            if count > 0 {
                record_lfs += count_lines(&chunk[segment..n], encoding);
                append_bounded(&mut raw, &chunk[segment..n], options.max_record_bytes);
                segment = n;
            }
            let record = report.records + 1;
            if !omitted.contains(&record) {
                process_record(
                    &mut report,
                    &raw,
                    start..end,
                    line,
                    &mut first_rows,
                    &mut expected_width,
                    sink,
                )?;
            }
            line += record_lfs;
            record_lfs = 0;
            // For an oversized record, count remaining line breaks while chunks are read below.
            raw.clear();
            start = end;
            report.records += 1;
        }
        let consumed = match &result {
            Err(QuarryError::Parse(e)) => {
                e.offset.saturating_sub(absolute).min(count as u64) as usize
            }
            Err(QuarryError::EncodingAt { offset, .. }) => {
                offset.saturating_sub(absolute).min(count as u64) as usize
            }
            _ => count,
        };
        if count > 0 && segment < consumed {
            record_lfs += count_lines(&chunk[segment..consumed], encoding);
            append_bounded(
                &mut raw,
                &chunk[segment..consumed],
                options.max_record_bytes,
            );
        }
        absolute += consumed as u64;
        report.bytes_scanned = absolute;
        progress.bytes.store(absolute, Ordering::Release);
        progress.records.store(report.records, Ordering::Release);
        if let Err(e) = result {
            let column = if raw.len() as u64 >= absolute.saturating_sub(start) {
                let bytes = if start == 0 {
                    encoding.strip_bom(&raw).1
                } else {
                    &raw
                };
                column_at(bytes, delimiter, encoding)
            } else {
                None
            };
            let error_line = line + record_lfs;
            report.add(error_diagnostic(
                &e,
                Some(report.records + 1),
                column,
                start,
                error_line,
                None,
            ));
            report.status = InspectionStatus::Stopped;
            break;
        }
        if count == 0 {
            break;
        }
    }
    if options.header_mode == HeaderMode::Auto && first_rows.len() >= 2 {
        report.has_header = crate::detect_header(&first_rows);
        report.header_certain = true;
    }
    for d in &mut report.findings {
        d.data_row = d
            .record
            .and_then(|r| r.checked_sub(u64::from(report.has_header)))
            .filter(|r| *r > 0);
        if report.has_header && d.record == Some(1) {
            d.data_row = None;
        }
    }
    if !source_matches_stamp(file, path, &report.stamp)? {
        return Err(QuarryError::SourceChanged);
    }
    Ok(report)
}
fn append_bounded(out: &mut Vec<u8>, bytes: &[u8], limit: usize) {
    let n = bytes.len().min(limit.saturating_sub(out.len()));
    out.extend_from_slice(&bytes[..n]);
}
fn count_lines(bytes: &[u8], encoding: Encoding) -> u64 {
    if encoding.is_utf16() {
        bytes
            .chunks_exact(2)
            .filter(|b| {
                if encoding == Encoding::Utf16Le {
                    b[0] == b'\n' && b[1] == 0
                } else {
                    b[0] == 0 && b[1] == b'\n'
                }
            })
            .count() as u64
    } else {
        memchr::memchr_iter(b'\n', bytes).count() as u64
    }
}
fn column_at(bytes: &[u8], delimiter: u8, encoding: Encoding) -> Option<usize> {
    let mut column = 1;
    // States mirror the authoritative record scanner for a known-valid prefix:
    // start, unquoted, quoted, after quote, CR following a closing quote.
    let mut state = 0;
    let width = if encoding.is_utf16() { 2 } else { 1 };
    for p in bytes.chunks_exact(width) {
        let b = if width == 1 {
            p[0]
        } else if encoding == Encoding::Utf16Le {
            if p[1] == 0 { p[0] } else { 0x80 }
        } else if p[0] == 0 {
            p[1]
        } else {
            0x80
        };
        state = match (state, b) {
            (0, b'"') => 2,
            (0 | 1, b) if b == delimiter => {
                column += 1;
                0
            }
            (0, _) => 1,
            (1, _) => 1,
            (2, b'"') => 3,
            (2, _) => 2,
            (3, b'"') => 2,
            (3, b) if b == delimiter => {
                column += 1;
                0
            }
            (3, b'\r') => 4,
            (3, b'\n') | (4, b'\n') => 0,
            _ => state,
        };
    }
    Some(column)
}

fn error_diagnostic(
    e: &QuarryError,
    record: Option<u64>,
    column: Option<usize>,
    start: u64,
    line: u64,
    range: Option<Range<u64>>,
) -> Diagnostic {
    let (kind, message, offset, advice) = match e {
        QuarryError::Parse(p) => (
            DiagnosticKind::Structure,
            match p.kind {
                ParseErrorKind::UnexpectedQuote => {
                    "A quote appears inside an unquoted field.".into()
                }
                ParseErrorKind::UnexpectedAfterQuote(_) => {
                    "Unexpected text follows a closing quote.".into()
                }
                ParseErrorKind::UnterminatedQuote => {
                    "A quoted field reaches the end of the file without a closing quote.".into()
                }
                ParseErrorKind::ExpectedLfAfterCr => {
                    "A quoted record has an incomplete line ending.".into()
                }
                _ => p.to_string(),
            },
            Some(p.offset),
            "Check the delimiter and quoting, correct the source, then retry. Record boundaries after this error are not assumed.",
        ),
        QuarryError::EncodingAt { offset, .. } => (
            DiagnosticKind::Encoding,
            e.to_string(),
            Some(*offset),
            "Choose the correct encoding and retry. No characters were replaced.",
        ),
        QuarryError::Encoding(_) => (
            DiagnosticKind::Encoding,
            e.to_string(),
            None,
            "Choose the correct encoding and retry. No characters were replaced.",
        ),
        QuarryError::RecordTooLarge { .. } => (
            DiagnosticKind::Limit,
            e.to_string(),
            Some(start),
            "The existing safety limit has not been raised. Omit this record only after its full boundary is known.",
        ),
        _ => (
            DiagnosticKind::Operation,
            e.to_string(),
            None,
            "Correct the cause and retry. The current document is preserved.",
        ),
    };
    Diagnostic {
        kind,
        message,
        advice: advice.into(),
        record,
        data_row: None,
        column,
        physical_line: Some(line),
        byte_offset: offset,
        record_range: range,
        edited: false,
        blocking: true,
    }
}

#[allow(clippy::too_many_arguments)]
fn process_record(
    report: &mut InspectionReport,
    raw: &[u8],
    range: Range<u64>,
    line: u64,
    first_rows: &mut Vec<Row>,
    expected_width: &mut Option<usize>,
    sink: &mut RecordSink<'_>,
) -> Result<(), QuarryError> {
    let record = report.records + 1;
    if range.end - range.start > report.options.max_record_bytes as u64 {
        report.add(error_diagnostic(
            &QuarryError::RecordTooLarge {
                limit: report.options.max_record_bytes,
            },
            Some(record),
            None,
            range.start,
            line,
            Some(range.clone()),
        ));
        sink(record, raw, None)?;
        return Ok(());
    }
    let (prefix, body) = if record == 1 {
        report.encoding.strip_bom(raw)
    } else {
        (&[][..], raw)
    };
    let bom_len = if !prefix.is_empty() {
        report.encoding.bom().len()
    } else {
        0
    };
    let decoded = match report
        .encoding
        .decode(body, report.options.max_record_bytes)
    {
        Ok(d) => d,
        Err(e) => {
            let mut d = error_diagnostic(
                &e,
                Some(record),
                None,
                range.start,
                line,
                Some(range.clone()),
            );
            if let Some(local) = d.byte_offset {
                let n = (local as usize).min(body.len());
                d.column = column_at(&body[..n], report.delimiter, report.encoding);
                d.physical_line = Some(line + count_lines(&body[..n], report.encoding));
            }
            d.byte_offset = d.byte_offset.map(|v| v + range.start + bom_len as u64);
            report.add(d);
            sink(record, raw, None)?;
            return Ok(());
        }
    };
    let parsed = match quarry_delimited::parse_record_with_field_limit(
        &decoded,
        report.delimiter,
        FIELD_LIMIT,
    ) {
        Ok(p) => p,
        Err(e) => {
            let mut d = error_diagnostic(
                &QuarryError::Parse(e),
                Some(record),
                None,
                range.start,
                line,
                Some(range.clone()),
            );
            if matches!(e.kind, ParseErrorKind::FieldLimitExceeded(_)) {
                d.kind = DiagnosticKind::Limit;
                d.advice = "This record exceeds the field-count safety limit used by inspection and structural operations. No fields were truncated. Review the source or explicitly omit this complete data record in a recovery copy.".into();
            }
            // A decoded-byte parse position cannot be advertised as an encoded source offset.
            d.byte_offset = None;
            report.add(d);
            sink(record, raw, None)?;
            return Ok(());
        }
    };
    let mut fields: Vec<Vec<u8>> = parsed.iter().map(|f| f.to_vec()).collect();
    if record <= 2 {
        first_rows.push(Row {
            offset: range.start,
            fields: fields.clone(),
        });
    }
    let mut changed = BTreeSet::new();
    if record == 1 && report.options.header_mode != HeaderMode::NoHeader {
        for (&c, v) in &report.edits.headers {
            if let Some(f) = fields.get_mut(c) {
                *f = v.clone();
                changed.insert(c);
            }
        }
    }
    for (&(r, c), v) in report
        .edits
        .cells
        .range((record - 1, 0)..=(record - 1, usize::MAX))
    {
        if r == record - 1
            && let Some(f) = fields.get_mut(c)
        {
            *f = v.clone();
            changed.insert(c);
        }
    }
    if record == 1 {
        *expected_width = Some(fields.len());
    }
    if let Some(expected) = *expected_width
        && fields.len() != expected
    {
        report.add(Diagnostic{kind:DiagnosticKind::RowWidth,message:format!("This record has {} field{}; the reference record has {expected}.",fields.len(),if fields.len()==1{""}else{"s"}),
            advice:"Uneven rows are supported. Review this difference; no fields were added or removed.".into(),record:Some(record),data_row:None,
            column:None,physical_line:Some(line),byte_offset:Some(range.start),record_range:Some(range.clone()),edited:false,blocking:false});
    }
    let field_starts = encoded_field_starts(body, report.delimiter);
    for (c, f) in fields.iter().enumerate() {
        if std::str::from_utf8(f).is_err() {
            let edited = changed.contains(&c);
            report.add(Diagnostic{kind:DiagnosticKind::Encoding,message:"This field contains bytes that are not valid UTF-8 text.".into(),
                advice:"Choose the correct encoding or explicitly omit the complete record in a recovery copy. The original value remains read-only.".into(),
                record:Some(record),data_row:None,column:Some(c+1),physical_line:Some(line),
                // Quoted unescaping changes offsets; never infer an offset from decoded field lengths.
                byte_offset:if !edited && report.encoding==Encoding::Utf8 {
                    field_starts.get(c).and_then(|start|{
                        std::str::from_utf8(&body[*start..]).err()
                            .map(|bad|range.start+bom_len as u64+(*start+bad.valid_up_to()) as u64)
                    })
                }else{None},
                record_range:Some(range.clone()),edited,blocking:true});
        } else if changed.contains(&c)
            && let Err(e) = report.encoding.encode(f, report.options.max_record_bytes)
        {
            let mut d = error_diagnostic(
                &e,
                Some(record),
                Some(c + 1),
                range.start,
                line,
                Some(range.clone()),
            );
            d.edited = true;
            d.byte_offset = None;
            report.add(d);
        }
    }
    for d in report
        .findings
        .iter_mut()
        .rev()
        .take_while(|d| d.record == Some(record))
    {
        if d.edited {
            d.physical_line = None;
        } else if let Some(offset) = d.byte_offset {
            let local = offset.saturating_sub(range.start) as usize;
            if local <= raw.len() {
                d.physical_line = Some(line + count_lines(&raw[..local], report.encoding));
            }
        }
    }
    let output = if changed.is_empty() {
        None
    } else {
        match serialize(
            &fields,
            report.delimiter,
            report.encoding,
            raw,
            record == 1,
            report.options.max_record_bytes,
        ) {
            Ok(out) => Some(out),
            Err(e) => {
                let mut d = error_diagnostic(
                    &e,
                    Some(record),
                    None,
                    range.start,
                    line,
                    Some(range.clone()),
                );
                d.edited = true;
                d.byte_offset = None;
                report.add(d);
                None
            }
        }
    };
    sink(record, raw, output.as_deref())?;
    Ok(())
}

fn encoded_field_starts(bytes: &[u8], delimiter: u8) -> Vec<usize> {
    let mut starts = vec![0];
    let mut state = 0;
    for (i, &b) in bytes.iter().enumerate() {
        state = match (state, b) {
            (0, b'"') => 2,
            (0 | 1, b) if b == delimiter => {
                starts.push(i + 1);
                0
            }
            (0, _) => 1,
            (1, _) => 1,
            (2, b'"') => 3,
            (2, _) => 2,
            (3, b'"') => 2,
            (3, b) if b == delimiter => {
                starts.push(i + 1);
                0
            }
            _ => state,
        };
    }
    starts
}

fn serialize(
    fields: &[Vec<u8>],
    delimiter: u8,
    encoding: Encoding,
    original: &[u8],
    first: bool,
    limit: usize,
) -> Result<Vec<u8>, QuarryError> {
    let (_, body) = if first {
        encoding.strip_bom(original)
    } else {
        (&[][..], original)
    };
    let decoded = encoding.decode(body, limit)?;
    let ending = if decoded.ends_with(b"\r\n") {
        b"\r\n".as_slice()
    } else if decoded.ends_with(b"\n") {
        b"\n".as_slice()
    } else {
        b"".as_slice()
    };
    let mut bytes = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            bytes.push(delimiter);
        }
        let quoted = f
            .iter()
            .any(|b| [delimiter, b'"', b'\r', b'\n'].contains(b))
            || (i == 0 && f.starts_with(crate::UTF8_BOM))
            || (fields.len() == 1 && f.is_empty() && ending.is_empty());
        if quoted {
            bytes.push(b'"');
        }
        for &b in f {
            bytes.push(b);
            if quoted && b == b'"' {
                bytes.push(b);
            }
        }
        if quoted {
            bytes.push(b'"');
        }
        if bytes.len() > limit.saturating_mul(3) {
            return Err(QuarryError::RecordTooLarge { limit });
        }
    }
    bytes.extend_from_slice(ending);
    let encoded = encoding.encode(&bytes, limit)?;
    let prefix = if first && !encoding.strip_bom(original).0.is_empty() {
        encoding.bom()
    } else {
        &[]
    };
    if prefix.len() + encoded.len() > limit {
        return Err(QuarryError::RecordTooLarge { limit });
    }
    let mut out = prefix.to_vec();
    out.extend_from_slice(&encoded);
    Ok(out)
}
fn escape_bytes(bytes: &[u8], encoding: Encoding, highlight: Option<usize>) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let mut token = String::new();
        let (n, decoded) = if encoding.is_utf16() {
            [2, 4]
                .into_iter()
                .filter(|n| i + n <= bytes.len())
                .find_map(|n| {
                    encoding
                        .decode(&bytes[i..i + n], 16)
                        .ok()
                        .map(|v| (n, String::from_utf8_lossy(&v).into_owned()))
                })
                .map_or((2.min(bytes.len() - i), None), |(n, s)| (n, Some(s)))
        } else if encoding == Encoding::Utf8 {
            (1..=4)
                .filter(|n| i + n <= bytes.len())
                .find_map(|n| {
                    std::str::from_utf8(&bytes[i..i + n])
                        .ok()
                        .map(|s| (n, s.to_owned()))
                })
                .map_or((1, None), |(n, s)| (n, Some(s)))
        } else {
            (
                1,
                encoding
                    .decode(&bytes[i..i + 1], 16)
                    .ok()
                    .map(|v| String::from_utf8_lossy(&v).into_owned()),
            )
        };
        if let Some(text) = decoded {
            for c in text.chars() {
                push_visible(&mut token, c);
            }
        } else {
            for b in &bytes[i..i + n] {
                token += &format!("\\x{b:02X}");
            }
        }
        let marked = highlight.is_some_and(|h| h >= i && h < i + n);
        if marked {
            out.push('⟦');
        }
        out += &token;
        if marked {
            out.push('⟧');
        }
        i += n;
    }
    if highlight == Some(bytes.len()) {
        out.push_str("⟦EOF⟧");
    }
    out
}

fn push_visible(out: &mut String, c: char) {
    match c {
        '\n' => out.push_str("[LF]\n"),
        '\r' => out.push_str("[CR]"),
        '\t' => out.push_str("[TAB]"),
        c if c.is_control() => *out += &format!("\\u{{{:X}}}", c as u32),
        c => out.push(c),
    }
}

#[derive(Clone, Debug)]
pub struct RecoverySummary {
    pub destination: PathBuf,
    pub removed_records: u64,
    pub retained_records: u64,
    pub bytes_written: u64,
}
pub struct RecoveryJob {
    shared: Arc<Progress>,
    handle: Option<JoinHandle<Result<Option<RecoverySummary>, QuarryError>>>,
}
impl RecoveryJob {
    pub fn start(
        report: &InspectionReport,
        selection: &BTreeSet<usize>,
        destination: impl AsRef<Path>,
    ) -> Result<Self, QuarryError> {
        let records = report.selected_records(selection)?;
        if !report.is_current() {
            return Err(QuarryError::SourceChanged);
        }
        let report = report.clone();
        let destination = destination.as_ref().to_owned();
        let source = File::open(&report.path)?;
        let edit_bytes = report
            .edits
            .headers
            .values()
            .chain(report.edits.cells.values())
            .fold(0u64, |sum, v| sum.saturating_add(v.len() as u64));
        crate::check_storage(
            destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
            report
                .total_bytes
                .saturating_add(edit_bytes)
                .saturating_mul(3),
        )?;
        let output = ExportTarget::new_guarded(
            &report.path,
            destination.clone(),
            &source,
            report.stamp.clone(),
        )?;
        let shared = Arc::new(Progress::new());
        let worker = Arc::clone(&shared);
        worker.total.store(report.total_bytes, Ordering::Release);
        worker.phase.store(1, Ordering::Release);
        let handle = thread::spawn(move || {
            let result = recover(report, records, destination, source, output, &worker);
            worker.done.store(true, Ordering::Release);
            result
        });
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }
    pub fn progress(&self) -> InspectionProgress {
        self.shared.snapshot()
    }
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Release);
    }
    pub fn wait(mut self) -> Result<Option<RecoverySummary>, QuarryError> {
        self.handle
            .take()
            .unwrap()
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}
impl Drop for RecoveryJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
fn recover(
    report: InspectionReport,
    omitted: BTreeSet<u64>,
    destination: PathBuf,
    mut source: File,
    mut output: ExportTarget,
    progress: &Progress,
) -> Result<Option<RecoverySummary>, QuarryError> {
    let mut prefix = vec![0; report.encoding.bom().len()];
    let n = source.read(&mut prefix)?;
    let bom = prefix[..n].starts_with(report.encoding.bom()) && !report.encoding.bom().is_empty();
    if bom {
        output.write_raw_all(report.encoding.bom())?;
    }
    let mut retained = 0;
    let result = scan(
        &mut source,
        &report.path,
        report.stamp.clone(),
        InspectionOptions {
            header_mode: if report.has_header {
                HeaderMode::FirstRow
            } else {
                HeaderMode::NoHeader
            },
            ..report.options
        },
        report.edits.clone(),
        progress,
        &omitted,
        &mut |record, raw, edited| {
            if progress.cancel.load(Ordering::Acquire) {
                return Ok(());
            }
            let bytes = edited.unwrap_or(raw);
            let bytes = if record == 1 && bom {
                report.encoding.strip_bom(bytes).1
            } else {
                bytes
            };
            if retained == 0
                && !bom
                && !report.encoding.bom().is_empty()
                && bytes.starts_with(report.encoding.bom())
            {
                let values =
                    crate::parse_encoded_record(bytes, report.delimiter, 1, report.encoding)?
                        .into_iter()
                        .map(|f| f.into_owned())
                        .collect::<Vec<_>>();
                let protected = serialize(
                    &values,
                    report.delimiter,
                    report.encoding,
                    bytes,
                    false,
                    report.options.max_record_bytes,
                )?;
                output.write_raw_all(&protected)?;
            } else {
                output.write_raw_all(bytes)?;
            }
            retained += 1;
            Ok(())
        },
    )?;
    if result.status == InspectionStatus::Cancelled {
        return Ok(None);
    }
    if result.status != InspectionStatus::Complete || result.blocking_findings > 0 {
        return Err(QuarryError::InvalidOption(
            "recovery still contains blocking data problems; inspect and correct or select those complete records first",
        ));
    }
    if progress.cancel.load(Ordering::Acquire) {
        return Ok(None);
    }
    let staged = output.flush_for_inspection()?;
    progress.phase.store(2, Ordering::Release);
    progress.bytes.store(0, Ordering::Release);
    let mut staged_file = File::open(&staged)?;
    let stamp = SourceStamp::from_file(&staged_file)?;
    progress.total.store(stamp.file_size(), Ordering::Release);
    let validated = scan(
        &mut staged_file,
        &staged,
        stamp,
        InspectionOptions {
            delimiter: Some(report.delimiter),
            header_mode: if report.has_header {
                HeaderMode::FirstRow
            } else {
                HeaderMode::NoHeader
            },
            encoding: Some(report.encoding),
            ..report.options
        },
        InspectionEdits::default(),
        progress,
        &BTreeSet::new(),
        &mut |_, _, _| Ok(()),
    )?;
    if validated.status == InspectionStatus::Cancelled {
        return Ok(None);
    }
    if validated.status != InspectionStatus::Complete || validated.blocking_findings > 0 {
        return Err(QuarryError::InvalidOption(
            "recovery copy failed validation; no output was published",
        ));
    }
    let bytes = output.bytes_written();
    match output.publish(retained, bytes, &progress.cancel)? {
        crate::FilterExportOutcome::Cancelled => Ok(None),
        crate::FilterExportOutcome::Complete(_) => Ok(Some(RecoverySummary {
            destination,
            removed_records: omitted.len() as u64,
            retained_records: retained,
            bytes_written: bytes,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OpenOptions;
    fn options() -> InspectionOptions {
        InspectionOptions {
            delimiter: Some(b','),
            header_mode: HeaderMode::FirstRow,
            ..Default::default()
        }
    }
    fn inspect(
        bytes: &[u8],
        opts: InspectionOptions,
        edits: InspectionEdits,
    ) -> (tempfile::TempDir, InspectionReport) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.csv");
        std::fs::write(&path, bytes).unwrap();
        let report = InspectionJob::start(&path, opts, edits)
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        (dir, report)
    }
    #[test]
    fn quoted_multiline_and_ragged_rows_have_correct_locations() {
        let raw = b"name,note\r\nA,\"one\r\ntwo\"\r\nB\r\nC,x,y\r\n";
        let (_dir, r) = inspect(raw, options(), InspectionEdits::default());
        assert_eq!(r.status, InspectionStatus::Complete);
        assert_eq!(r.records, 4);
        assert_eq!(r.findings.len(), 2);
        assert_eq!(
            (
                r.findings[0].record,
                r.findings[0].data_row,
                r.findings[0].physical_line
            ),
            (Some(3), Some(2), Some(4))
        );
        assert!(
            r.findings
                .iter()
                .all(|d| d.kind == DiagnosticKind::RowWidth && !d.blocking)
        );
    }
    #[test]
    fn malformed_quoting_stops_without_recovery_or_guessed_ranges() {
        for raw in [
            b"a,b\n1,2\n3,ba\"d\n4,5\n".as_slice(),
            b"a,b\n1,2\n3,\"bad\n4,5\n",
        ] {
            let (_dir, r) = inspect(raw, options(), InspectionEdits::default());
            assert_eq!(r.status, InspectionStatus::Stopped);
            let d = r.findings.last().unwrap();
            assert_eq!(d.kind, DiagnosticKind::Structure);
            assert_eq!(d.record, Some(3));
            assert!(d.record_range.is_none());
            assert!(!r.can_omit(0));
            assert!(r.preview(r.findings.len() - 1).unwrap().text.contains('⟦'));
        }
    }
    #[test]
    fn every_encoding_uses_source_bytes_and_multiline_positions() {
        for encoding in Encoding::ALL {
            let text = "a,b\r\n1,\"é\r\nx\"\r\n2,3\r\n";
            let mut raw = encoding.bom().to_vec();
            raw.extend_from_slice(&encoding.encode(text.as_bytes(), 4096).unwrap());
            let (_dir, r) = inspect(
                &raw,
                InspectionOptions {
                    encoding: Some(encoding),
                    chunk_bytes: 2,
                    ..options()
                },
                InspectionEdits::default(),
            );
            assert_eq!(r.status, InspectionStatus::Complete, "{encoding}");
            assert_eq!(r.records, 3, "{encoding}");
            assert!(r.findings.is_empty(), "{encoding}: {:?}", r.findings);
            assert_eq!(r.bytes_scanned, raw.len() as u64);
        }
    }
    #[test]
    fn invalid_encodings_are_located_beyond_the_first_rows() {
        for (encoding, bad) in [
            (Encoding::Utf8, vec![0xff]),
            (Encoding::Windows1252, vec![0x81]),
            (Encoding::Utf16Le, vec![0, 0xd8]),
            (Encoding::Utf16Be, vec![0xd8, 0]),
        ] {
            let mut raw = encoding.bom().to_vec();
            raw.extend_from_slice(&encoding.encode(b"a,b\n1,2\n3,", 4096).unwrap());
            let offset = raw.len() as u64;
            raw.extend(bad);
            raw.extend_from_slice(&encoding.encode(b"\n", 4096).unwrap());
            let (_dir, r) = inspect(
                &raw,
                InspectionOptions {
                    encoding: Some(encoding),
                    ..options()
                },
                InspectionEdits::default(),
            );
            let d = r
                .findings
                .iter()
                .find(|d| d.kind == DiagnosticKind::Encoding)
                .unwrap();
            assert_eq!(d.record, Some(3), "{encoding}: {d:?}");
            assert_eq!(d.column, Some(2), "{encoding}: {d:?}");
            assert_eq!(d.byte_offset, Some(offset), "{encoding}: {d:?}");
        }
    }
    #[test]
    fn report_cap_does_not_stop_scanning_or_hide_coverage() {
        let mut raw = b"a,b\n".to_vec();
        for _ in 0..1100 {
            raw.extend_from_slice(b"x\n");
        }
        let (_dir, r) = inspect(&raw, options(), InspectionEdits::default());
        assert_eq!(r.findings.len(), 1000);
        assert_eq!(r.total_findings, 1100);
        assert_eq!(r.records, 1101);
        assert_eq!(r.status, InspectionStatus::Complete);
        assert_eq!(r.bytes_scanned, raw.len() as u64);
    }
    #[test]
    fn oversized_records_are_bounded_and_keep_later_line_locations() {
        let raw = b"a,b\n12345678901234567890\nx\n";
        let (_dir, r) = inspect(
            raw,
            InspectionOptions {
                max_record_bytes: 8,
                chunk_bytes: 3,
                ..options()
            },
            InspectionEdits::default(),
        );
        assert_eq!(r.status, InspectionStatus::Complete);
        assert_eq!(r.findings[0].kind, DiagnosticKind::Limit);
        assert_eq!(r.findings[1].physical_line, Some(3));
        assert!(r.can_omit(0));
    }
    #[test]
    fn edited_findings_do_not_invent_source_byte_positions() {
        let edits = InspectionEdits {
            cells: BTreeMap::from([((1, 1), vec![0xff])]),
            ..Default::default()
        };
        let (_dir, r) = inspect(b"a,b\n1,2\n", options(), edits.clone());
        let d = r.findings.iter().find(|d| d.edited).unwrap();
        assert_eq!(d.column, Some(2));
        assert_eq!(d.byte_offset, None);
        assert!(r.preview(0).unwrap().text.contains("Unsaved"));
        assert!(r.matches_edits(&edits));
        assert!(!r.matches_edits(&InspectionEdits::default()));
    }
    #[test]
    fn recovery_omits_unique_rows_and_preserves_native_bytes_and_edits() {
        for encoding in Encoding::ALL {
            let mut raw = encoding.bom().to_vec();
            raw.extend_from_slice(
                &encoding
                    .encode(b"a,b\r\n1,2\r\nbad\r\n3,4\r\n", 4096)
                    .unwrap(),
            );
            let edits = InspectionEdits {
                cells: BTreeMap::from([((1, 1), b"edited".to_vec())]),
                ..Default::default()
            };
            let (dir, r) = inspect(
                &raw,
                InspectionOptions {
                    encoding: Some(encoding),
                    ..options()
                },
                edits,
            );
            let selection = BTreeSet::from([r
                .findings
                .iter()
                .position(|d| d.kind == DiagnosticKind::RowWidth)
                .unwrap()]);
            let destination = dir.path().join("out.csv");
            let result = RecoveryJob::start(&r, &selection, &destination)
                .unwrap()
                .wait()
                .unwrap()
                .unwrap();
            assert_eq!(result.removed_records, 1);
            let mut expected = encoding.bom().to_vec();
            expected.extend_from_slice(
                &encoding
                    .encode(b"a,b\r\n1,edited\r\n3,4\r\n", 4096)
                    .unwrap(),
            );
            assert_eq!(std::fs::read(destination).unwrap(), expected, "{encoding}");
            assert_eq!(std::fs::read(&r.path).unwrap(), raw);
        }
    }
    #[test]
    fn recovery_can_remove_invalid_utf8_but_not_leave_other_blocking_rows() {
        let (dir, r) = inspect(
            b"a,b\n1,\xff\n2,\xff\n",
            options(),
            InspectionEdits::default(),
        );
        let destination = dir.path().join("out.csv");
        assert!(
            RecoveryJob::start(&r, &BTreeSet::from([0]), &destination)
                .unwrap()
                .wait()
                .is_err()
        );
        assert!(!destination.exists());
        RecoveryJob::start(&r, &BTreeSet::from([0, 1]), &destination)
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"a,b\n");
        assert!(RecoveryJob::start(&r, &BTreeSet::from([0]), &destination).is_err());
    }
    #[test]
    fn recovery_cannot_turn_a_leading_data_bom_into_a_signature() {
        let (dir, r) = inspect(
            b"\xff\n\xef\xbb\xbfx\n",
            InspectionOptions {
                header_mode: HeaderMode::NoHeader,
                ..options()
            },
            InspectionEdits::default(),
        );
        let out = dir.path().join("out.csv");
        RecoveryJob::start(&r, &BTreeSet::from([0]), &out)
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        let session = crate::Session::open(
            &out,
            OpenOptions {
                header_mode: HeaderMode::NoHeader,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(session.first_rows[0].fields[0], b"\xef\xbb\xbfx");
    }
    #[test]
    fn stale_source_rejects_preview_and_recovery() {
        let (dir, r) = inspect(b"a,b\nx\n", options(), InspectionEdits::default());
        std::fs::write(&r.path, b"a,b\n1,2\n").unwrap();
        assert!(!r.is_current());
        assert!(r.preview(0).is_err());
        assert!(RecoveryJob::start(&r, &BTreeSet::from([0]), dir.path().join("out.csv")).is_err());
    }
    #[test]
    fn cancelled_scan_and_recovery_do_not_publish() {
        let mut raw = b"a,b\nx\n".to_vec();
        for _ in 0..20000 {
            raw.extend_from_slice(b"1,2\n");
        }
        let (dir, r) = inspect(&raw, options(), InspectionEdits::default());
        let out = dir.path().join("out.csv");
        let job = RecoveryJob::start(&r, &BTreeSet::from([0]), &out).unwrap();
        job.cancel();
        assert!(job.wait().unwrap().is_none());
        assert!(!out.exists());
        let scan = InspectionJob::start(
            &r.path,
            InspectionOptions {
                chunk_bytes: 2,
                ..options()
            },
            InspectionEdits::default(),
        )
        .unwrap();
        scan.cancel();
        assert_eq!(scan.wait().unwrap().status, InspectionStatus::Cancelled);
    }
    #[test]
    fn preview_is_bounded_escaped_and_highlights_only_once() {
        let (dir, r) = inspect(b"a,b\n1,\xff\n", options(), InspectionEdits::default());
        let preview = r.preview(0).unwrap();
        assert!(preview.end - preview.start <= 8192);
        assert_eq!(preview.text.matches('⟦').count(), 1);
        assert_eq!(preview.text.matches('⟧').count(), 1);
        assert!(preview.text.contains("\\xFF"));
        drop(dir);
    }
}

#[cfg(test)]
mod location_regressions {
    use super::*;
    fn report(raw: &[u8], encoding: Encoding) -> InspectionReport {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.csv");
        std::fs::write(&p, raw).unwrap();
        InspectionJob::start(
            p,
            InspectionOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::FirstRow,
                encoding: Some(encoding),
                ..Default::default()
            },
            InspectionEdits::default(),
        )
        .unwrap()
        .wait()
        .unwrap()
    }
    #[test]
    fn odd_utf16_tail_retains_the_preceding_record_locations() {
        let mut raw = vec![0xff, 0xfe];
        raw.extend_from_slice(
            &"a,b\n1,2\n3,"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        );
        let offset = raw.len() as u64;
        raw.push(0xaa);
        let r = report(&raw, Encoding::Utf16Le);
        let d = r.findings.last().unwrap();
        assert_eq!(r.records, 2);
        assert_eq!(d.record, Some(3));
        assert_eq!(d.byte_offset, Some(offset));
        assert_eq!(d.column, Some(2));
    }
    #[test]
    fn escaped_quotes_do_not_shift_the_error_column() {
        let r = report(b"a,b\n1,\"x\"\"y,z\",u\"v\n", Encoding::Utf8);
        let d = r.findings.last().unwrap();
        assert_eq!(d.record, Some(2));
        assert_eq!(d.column, Some(3));
    }
}

#[cfg(test)]
mod diagnostic_location_tests {
    use super::*;
    #[test]
    fn invalid_byte_after_escaped_quote_and_line_break_has_original_location() {
        let raw = b"a,b\n1,\"first \"\"quote\"\"\n\xff\"\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.csv");
        std::fs::write(&path, raw).unwrap();
        let r = InspectionJob::start(
            path,
            InspectionOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
            InspectionEdits::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
        let d = &r.findings[0];
        assert_eq!(d.column, Some(2));
        assert_eq!(d.physical_line, Some(3));
        assert_eq!(
            d.byte_offset,
            Some(raw.iter().position(|b| *b == 0xff).unwrap() as u64)
        );
    }
    #[test]
    fn operation_locations_never_claim_generated_positions_are_source_offsets() {
        let d = Diagnostic::from_error(&QuarryError::InvalidNumericSortValue {
            data_row: 9,
            column: 3,
        });
        assert_eq!(d.data_row, Some(9));
        assert_eq!(d.column, Some(3));
        assert_eq!(d.byte_offset, None);
        let d = Diagnostic::from_error(&QuarryError::EncodingAt {
            encoding: Encoding::Utf16Le,
            offset: 12,
            reason: "test".into(),
        });
        assert_eq!(d.kind, DiagnosticKind::Encoding);
        assert_eq!(d.byte_offset, None);
    }
    #[test]
    fn record_decode_error_keeps_known_record_without_claiming_a_source_offset() {
        let error =
            crate::parse_encoded_record(&[0, 0xd8], b',', 8, Encoding::Utf16Le).unwrap_err();
        let d = Diagnostic::from_error(&error);
        assert_eq!(d.kind, DiagnosticKind::Encoding);
        assert_eq!(d.record, Some(9));
        assert_eq!(d.byte_offset, None);
        assert!(error.to_string().contains("Record 9"));
    }
    #[test]
    fn aligned_invalid_utf16_records_remain_recoverable_and_later_rows_are_scanned() {
        for encoding in [Encoding::Utf16Le, Encoding::Utf16Be] {
            for chunk in [2, 6, 64] {
                let dir = tempfile::tempdir().unwrap();
                let source = dir.path().join("bad-utf16.csv");
                let mut bytes = encoding.bom().to_vec();
                bytes.extend_from_slice(&encoding.encode(b"a,b\n1,", 1024).unwrap());
                let invalid_offset = bytes.len() as u64;
                bytes.extend_from_slice(if encoding == Encoding::Utf16Le {
                    &[0x00, 0xd8]
                } else {
                    &[0xd8, 0x00]
                });
                bytes.extend_from_slice(&encoding.encode(b"\n2,ok\n3\n", 1024).unwrap());
                std::fs::write(&source, &bytes).unwrap();
                let r = InspectionJob::start(
                    &source,
                    InspectionOptions {
                        delimiter: Some(b','),
                        header_mode: HeaderMode::FirstRow,
                        encoding: Some(encoding),
                        chunk_bytes: chunk,
                        ..Default::default()
                    },
                    InspectionEdits::default(),
                )
                .unwrap()
                .wait()
                .unwrap();
                assert_eq!(
                    r.status,
                    InspectionStatus::Complete,
                    "{encoding} chunk {chunk}"
                );
                assert_eq!(r.records, 4);
                let bad = r
                    .findings
                    .iter()
                    .position(|d| d.kind == DiagnosticKind::Encoding)
                    .unwrap();
                assert_eq!(r.findings[bad].byte_offset, Some(invalid_offset));
                assert_eq!(r.findings[bad].record, Some(2));
                assert!(r.can_omit(bad));
                assert!(r.findings.iter().any(|d| d.record == Some(4)));
                let destination = dir.path().join("recovered.csv");
                RecoveryJob::start(&r, &BTreeSet::from([bad]), &destination)
                    .unwrap()
                    .wait()
                    .unwrap()
                    .unwrap();
                let mut expected = encoding.bom().to_vec();
                expected.extend_from_slice(&encoding.encode(b"a,b\n2,ok\n3\n", 1024).unwrap());
                assert_eq!(std::fs::read(destination).unwrap(), expected);
                assert_eq!(std::fs::read(source).unwrap(), bytes);
            }
        }
    }
    #[test]
    fn invalid_utf16_inside_a_multiline_quoted_record_has_a_safe_complete_range() {
        for encoding in [Encoding::Utf16Le, Encoding::Utf16Be] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("quoted.csv");
            let mut raw = encoding.bom().to_vec();
            raw.extend_from_slice(&encoding.encode(b"a,b\r\n1,\"x,", 1024).unwrap());
            let offset = raw.len() as u64;
            raw.extend_from_slice(if encoding == Encoding::Utf16Le {
                &[0, 0xdc]
            } else {
                &[0xdc, 0]
            });
            raw.extend_from_slice(&encoding.encode(b"\r\ntail\"\r\n2,ok\r\n", 1024).unwrap());
            std::fs::write(&path, &raw).unwrap();
            assert!(
                crate::Session::open_with_encoding(
                    &path,
                    crate::OpenOptions::default(),
                    Some(encoding)
                )
                .is_err()
            );
            let report = InspectionJob::start(
                &path,
                InspectionOptions {
                    encoding: Some(encoding),
                    delimiter: Some(b','),
                    header_mode: HeaderMode::FirstRow,
                    chunk_bytes: 2,
                    ..Default::default()
                },
                InspectionEdits::default(),
            )
            .unwrap()
            .wait()
            .unwrap();
            assert_eq!(report.status, InspectionStatus::Complete);
            assert_eq!(report.records, 3);
            assert_eq!(report.findings[0].column, Some(2));
            assert_eq!(report.findings[0].physical_line, Some(2));
            assert_eq!(report.findings[0].byte_offset, Some(offset));
            assert!(report.can_omit(0));
            let out = dir.path().join("recovered.csv");
            RecoveryJob::start(&report, &BTreeSet::from([0]), &out)
                .unwrap()
                .wait()
                .unwrap()
                .unwrap();
            let mut expected = encoding.bom().to_vec();
            expected.extend_from_slice(&encoding.encode(b"a,b\r\n2,ok\r\n", 1024).unwrap());
            assert_eq!(std::fs::read(out).unwrap(), expected);
        }
    }
}
