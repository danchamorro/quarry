use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use memchr::{memchr, memmem::Finder};
use quarry_delimited::{RecordScanner, parse_record};

use crate::filter::{FilterQuery, matching_fields, validate_query};
use crate::{DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, QuarryError, Session, SourceStamp};

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

pub struct SaveAsJob {
    shared: Arc<SharedState>,
    handle: Option<JoinHandle<Result<SaveAsOutcome, QuarryError>>>,
}

enum SaveTarget {
    New(PathBuf),
    Source(SourceStamp),
}

impl SaveAsJob {
    fn start(
        source_path: PathBuf,
        file_size: u64,
        delimiter: u8,
        has_header: bool,
        header_renames: BTreeMap<usize, Vec<u8>>,
        target: SaveTarget,
        config: ExportConfig,
    ) -> Result<Self, QuarryError> {
        if config.chunk_bytes == 0 {
            return Err(QuarryError::InvalidOption("save-as chunk must be non-zero"));
        }
        if !has_header {
            return Err(QuarryError::InvalidOption(
                "header renames require a header row",
            ));
        }
        let source = File::open(&source_path)?;
        let output = match target {
            SaveTarget::New(destination) => ExportTarget::new(&source_path, destination)?,
            SaveTarget::Source(expected) => {
                ExportTarget::replace_source(&source_path, source.metadata()?, expected)?
            }
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
                    &header_renames,
                    config,
                    &worker_state,
                );
                match &result {
                    Ok(SaveAsOutcome::Cancelled) => {
                        worker_state.cancelled.store(true, Ordering::Release);
                    }
                    Err(error) => {
                        *worker_state.error.lock().unwrap() = Some(error.to_string());
                    }
                    Ok(SaveAsOutcome::Complete(_)) => {}
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

    pub fn wait(mut self) -> Result<SaveAsOutcome, QuarryError> {
        self.handle
            .take()
            .expect("save-as handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
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
            DEFAULT_EXPORT_CONFIG,
        )
    }

    pub fn start_save_as_with_header_renames(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        destination: impl AsRef<Path>,
    ) -> Result<SaveAsJob, QuarryError> {
        SaveAsJob::start(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            header_renames,
            SaveTarget::New(destination.as_ref().to_path_buf()),
            DEFAULT_EXPORT_CONFIG,
        )
    }

    pub fn start_save_with_header_renames(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
    ) -> Result<SaveAsJob, QuarryError> {
        SaveAsJob::start(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            self.dialect.has_header,
            header_renames,
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
    ReplaceSource {
        permissions: fs::Permissions,
        source_stamp: SourceStamp,
    },
}

struct ExportTarget {
    writer: Option<BufWriter<File>>,
    temporary: PathBuf,
    destination: PathBuf,
    publication: Publication,
}

impl ExportTarget {
    fn new(source: &Path, destination: PathBuf) -> Result<Self, QuarryError> {
        validate_destination(source, &destination)?;
        Self::create(destination, Publication::CreateNew)
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
        if let Publication::ReplaceSource { permissions, .. } = &publication {
            options.mode(permissions.mode());
        }
        for _ in 0..100 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(".quarry-export-{}-{id}.tmp", std::process::id()));
            match options.open(&temporary) {
                Ok(file) => {
                    if let Publication::ReplaceSource { permissions, .. } = &publication
                        && let Err(error) = file.set_permissions(permissions.clone())
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
        if let Publication::ReplaceSource { permissions, .. } = &self.publication {
            file.set_permissions(permissions.clone())?;
        }
        file.sync_all()?;
        drop(file);
        if cancel_requested.load(Ordering::Acquire) {
            self.remove_temporary()?;
            return Ok(FilterExportOutcome::Cancelled);
        }
        let publish_result = match &self.publication {
            Publication::CreateNew => publish_no_replace(&self.temporary, &self.destination),
            Publication::ReplaceSource { source_stamp, .. } => {
                let unchanged = fs::symlink_metadata(&self.destination)
                    .ok()
                    .filter(|metadata| !metadata.file_type().is_symlink())
                    .is_some_and(|metadata| SourceStamp::from_metadata(&metadata) == *source_stamp);
                if !unchanged {
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

fn run_save_as(
    mut source: File,
    mut output: ExportTarget,
    delimiter: u8,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    config: ExportConfig,
    shared: &SharedState,
) -> Result<SaveAsOutcome, QuarryError> {
    match copy_with_rewritten_header(
        &mut source,
        &mut output,
        delimiter,
        header_renames,
        config,
        shared,
    ) {
        Ok(Some(bytes_written)) => {
            match output.publish(0, bytes_written, &shared.cancel_requested)? {
                FilterExportOutcome::Complete(summary) => {
                    Ok(SaveAsOutcome::Complete(SaveAsSummary {
                        destination: summary.destination,
                        bytes_written: summary.bytes_written,
                    }))
                }
                FilterExportOutcome::Cancelled => Ok(SaveAsOutcome::Cancelled),
            }
        }
        Ok(None) => {
            output.discard()?;
            Ok(SaveAsOutcome::Cancelled)
        }
        Err(error) => {
            output.discard()?;
            Err(error)
        }
    }
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
    const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
    let (prefix, record) = record
        .strip_prefix(UTF8_BOM)
        .map_or((&[][..], record), |record| (UTF8_BOM, record));
    let fields = parse_record(record, delimiter)?;
    if header_renames
        .last_key_value()
        .is_some_and(|(column, _)| *column >= fields.len())
    {
        return Err(QuarryError::InvalidOption(
            "header rename column is out of range",
        ));
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
        let field = header_renames
            .get(&column)
            .map_or(field.as_ref(), Vec::as_slice);
        serialized_len = serialized_len.saturating_add(delimited_field_len(field, delimiter));
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
        let field = header_renames
            .get(&column)
            .map_or(field.as_ref(), Vec::as_slice);
        bytes_written =
            bytes_written.saturating_add(write_delimited_field(output, field, delimiter)?);
    }

    output.write_all(ending)?;
    Ok(bytes_written.saturating_add(ending.len() as u64))
}

fn delimited_field_len(field: &[u8], delimiter: u8) -> usize {
    let quotes = field.iter().filter(|byte| **byte == b'"').count();
    if quotes > 0
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
) -> Result<u64, QuarryError> {
    let needs_quotes = field
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
        ExportConfig, ExportTarget, FilterExportJob, FilterExportOutcome, SaveAsJob, SaveAsOutcome,
        SaveTarget, SharedState, WorkerCompletion,
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

    fn wait_until_save_done(job: &SaveAsJob) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !job.progress().done {
            assert!(Instant::now() < deadline, "save-as did not finish promptly");
            thread::yield_now();
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
        let job = SaveAsJob::start(
            source.clone(),
            session.file_size,
            b';',
            true,
            renames,
            SaveTarget::New(destination.clone()),
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
    fn save_replaces_the_source_and_preserves_permissions() {
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
            .start_save_with_header_renames(BTreeMap::from([(1, b"person".to_vec())]))
            .unwrap();

        wait_until_save_done(&job);
        let SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("save unexpectedly cancelled");
        };
        assert_eq!(summary.destination, source);
        assert_eq!(fs::read(&source).unwrap(), b"id,person\n1,Ada\n");
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
    fn save_rejects_a_source_changed_since_the_session_opened() {
        let source = fixture(b"id,name\n1,Ada\n");
        let session = session(&source, b',', HeaderMode::FirstRow);
        fs::write(&source, b"id,name\n1,Grace\n").unwrap();

        assert!(matches!(
            session.start_save_with_header_renames(BTreeMap::from([(0, b"ID".to_vec())])),
            Err(QuarryError::SourceChanged)
        ));
        assert_eq!(fs::read(&source).unwrap(), b"id,name\n1,Grace\n");
        assert!(temporary_exports(&source).is_empty());
        fs::remove_file(&source).unwrap();
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
        let job = SaveAsJob::start(
            source.clone(),
            session.file_size,
            b',',
            true,
            BTreeMap::from([(0, b"a renamed header".to_vec())]),
            SaveTarget::New(destination.clone()),
            ExportConfig {
                chunk_bytes: 4,
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
    fn cancelling_save_as_removes_partial_output() {
        let mut source_bytes = b"id,name\n".to_vec();
        source_bytes.extend_from_slice(&b"1,Ada\n".repeat(500_000));
        let source = fixture(&source_bytes);
        let destination = destination(&source, "cancelled-save.csv");
        let session = session(&source, b',', HeaderMode::FirstRow);
        let job = SaveAsJob::start(
            source.clone(),
            session.file_size,
            b',',
            true,
            BTreeMap::from([(0, b"ID".to_vec())]),
            SaveTarget::New(destination.clone()),
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
