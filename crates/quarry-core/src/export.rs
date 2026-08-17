use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use memchr::memmem::Finder;
use quarry_delimited::RecordScanner;

use crate::filter::{FilterQuery, matching_fields, validate_query};
use crate::{DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, QuarryError, Session};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct FilterExportConfig {
    chunk_bytes: usize,
    max_record_bytes: usize,
}

const DEFAULT_FILTER_EXPORT_CONFIG: FilterExportConfig = FilterExportConfig {
    chunk_bytes: DEFAULT_READ_CHUNK,
    max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
};

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
        config: FilterExportConfig,
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

impl Session {
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
            DEFAULT_FILTER_EXPORT_CONFIG,
        )
    }
}

struct ExportTarget {
    writer: Option<BufWriter<File>>,
    temporary: PathBuf,
    destination: PathBuf,
}

impl ExportTarget {
    fn new(source: &Path, destination: PathBuf) -> Result<Self, QuarryError> {
        validate_destination(source, &destination)?;
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        destination.file_name().ok_or(QuarryError::InvalidOption(
            "export destination must name a file",
        ))?;
        for _ in 0..100 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(".quarry-export-{}-{id}.tmp", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => {
                    return Ok(Self {
                        writer: Some(BufWriter::new(file)),
                        temporary,
                        destination,
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

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), QuarryError> {
        self.writer
            .as_mut()
            .expect("export writer is present")
            .write_all(bytes)?;
        Ok(())
    }

    fn publish(
        mut self,
        rows_written: u64,
        bytes_written: u64,
        cancel_requested: &AtomicBool,
    ) -> Result<FilterExportOutcome, QuarryError> {
        let mut writer = self.writer.take().expect("export writer is present");
        writer.flush()?;
        let file = writer.into_inner().map_err(|error| error.into_error())?;
        file.sync_all()?;
        if cancel_requested.load(Ordering::Acquire) {
            self.remove_temporary()?;
            return Ok(FilterExportOutcome::Cancelled);
        }
        if let Err(error) = publish_no_replace(&self.temporary, &self.destination) {
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

fn validate_destination(source: &Path, destination: &Path) -> Result<(), QuarryError> {
    let current_dir = std::env::current_dir()?;
    if normalize_path(source, &current_dir) == normalize_path(destination, &current_dir) {
        return Err(QuarryError::ExportDestinationIsSource);
    }
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            let same_path = fs::canonicalize(source)
                .ok()
                .zip(fs::canonicalize(destination).ok())
                .is_some_and(|(source, destination)| source == destination);
            Err(if same_path {
                QuarryError::ExportDestinationIsSource
            } else {
                QuarryError::ExportDestinationExists
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
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
    config: FilterExportConfig,
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

fn scan_export(
    source: &mut File,
    output: &mut ExportTarget,
    delimiter: u8,
    data_start: u64,
    query: &FilterQuery,
    config: FilterExportConfig,
    shared: &SharedState,
) -> Result<ScanOutcome, QuarryError> {
    let finders: Vec<_> = query
        .predicates
        .iter()
        .map(|predicate| Finder::new(&predicate.value))
        .collect();
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
                    &finders,
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
                        &finders,
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
    finders: &[Finder<'_>],
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
    if row_number < data_start || matching_fields(record, delimiter, query, finders)?.is_some() {
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
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        ExportTarget, FilterExportConfig, FilterExportJob, FilterExportOutcome, SharedState,
        WorkerCompletion,
    };
    use crate::{FilterOperator, FilterQuery, HeaderMode, OpenOptions, QuarryError, Session};

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
        FilterQuery::single(2, FilterOperator::Equals, b"keep".to_vec())
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

    #[test]
    fn exports_raw_header_and_matching_records_without_changing_the_source() {
        let source_bytes = b"id;note;kind\r\n1;\"line one\nline \"\"two\"\"\";keep\r\n2;plain;drop\r\n3;\"last;value\";keep";
        let expected =
            b"id;note;kind\r\n1;\"line one\nline \"\"two\"\"\";keep\r\n3;\"last;value\";keep";
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
            FilterExportConfig {
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
            FilterExportConfig {
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
