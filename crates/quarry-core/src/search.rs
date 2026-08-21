use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use memchr::memmem::Finder;
use quarry_delimited::RecordScanner;

use crate::{
    Checkpoint, DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, QuarryError, Session,
    StructuralIndex, parse_source_record,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchPosition {
    pub row: u64,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    pub row: u64,
    pub column: usize,
    pub record_offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOutcome {
    Match(SearchMatch),
    NotFound,
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchProgress {
    pub bytes_scanned: u64,
    pub rows_scanned: u64,
    pub total_bytes: u64,
    pub elapsed: Duration,
    pub done: bool,
    pub cancelled: bool,
}

struct SharedState {
    bytes_scanned: AtomicU64,
    rows_scanned: AtomicU64,
    finished_nanos: AtomicU64,
    done: AtomicBool,
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
            finished_nanos: AtomicU64::new(0),
            done: AtomicBool::new(false),
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

pub struct SearchJob {
    shared: Arc<SharedState>,
    handle: Option<JoinHandle<Result<SearchOutcome, QuarryError>>>,
}

impl SearchJob {
    fn start(
        path: PathBuf,
        file_size: u64,
        delimiter: u8,
        needle: Vec<u8>,
        start: SearchPosition,
        checkpoint: Checkpoint,
        max_record_bytes: usize,
    ) -> Result<Self, QuarryError> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(checkpoint.offset))?;
        let shared = Arc::new(SharedState::new(
            file_size.saturating_sub(checkpoint.offset),
        ));
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-search".into())
            .spawn(move || {
                let _completion = WorkerCompletion(&worker_state);
                let result = run_search(
                    file,
                    delimiter,
                    &needle,
                    start,
                    checkpoint,
                    max_record_bytes,
                    &worker_state,
                );
                if let Err(error) = &result {
                    *worker_state.error.lock().unwrap() = Some(error.to_string());
                }
                result
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> SearchProgress {
        let done = self.shared.done.load(Ordering::Acquire);
        let finished_nanos = self.shared.finished_nanos.load(Ordering::Acquire);
        SearchProgress {
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
        self.shared.cancelled.store(true, Ordering::Release);
    }

    pub fn wait(mut self) -> Result<SearchOutcome, QuarryError> {
        self.handle
            .take()
            .expect("search handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl Drop for SearchJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

impl Session {
    pub fn start_search(
        &self,
        index: &StructuralIndex,
        needle: Vec<u8>,
        start: SearchPosition,
    ) -> Result<SearchJob, QuarryError> {
        if needle.is_empty() {
            return Err(QuarryError::InvalidOption("search query must not be empty"));
        }
        let data_start = u64::from(self.dialect.has_header);
        let start = if start.row < data_start {
            SearchPosition {
                row: data_start,
                column: 0,
            }
        } else {
            start
        };
        let checkpoint = index.nearest_checkpoint(start.row);
        SearchJob::start(
            self.path.clone(),
            self.file_size,
            self.dialect.delimiter,
            needle,
            start,
            checkpoint,
            DEFAULT_MAX_RECORD_BYTES,
        )
    }
}

fn run_search(
    mut file: File,
    delimiter: u8,
    needle: &[u8],
    start: SearchPosition,
    checkpoint: Checkpoint,
    max_record_bytes: usize,
    shared: &SharedState,
) -> Result<SearchOutcome, QuarryError> {
    let finder = Finder::new(needle);
    let mut scanner = RecordScanner::at_offset(delimiter, checkpoint.offset)?;
    let mut chunk = vec![0; DEFAULT_READ_CHUNK];
    let mut absolute_start = checkpoint.offset;
    let mut row_number = checkpoint.row;
    let mut record_start = checkpoint.offset;
    let mut records_scanned = 0_u64;
    let mut record = Vec::new();

    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(SearchOutcome::Cancelled);
        }
        let read = file.read(&mut chunk)?;
        if read == 0 {
            let has_final_record = scanner.finish(absolute_start, |_| {})?;
            if has_final_record {
                records_scanned += 1;
                shared
                    .rows_scanned
                    .store(records_scanned, Ordering::Release);
                if row_number >= start.row {
                    if record.len() > max_record_bytes {
                        return Err(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    }
                    if let Some(found) =
                        find_record(&finder, &record, delimiter, row_number, record_start, start)?
                    {
                        return Ok(SearchOutcome::Match(found));
                    }
                }
            }
            shared
                .bytes_scanned
                .store(absolute_start - checkpoint.offset, Ordering::Release);
            return Ok(SearchOutcome::NotFound);
        }

        let mut segment_start = 0;
        let mut found = None;
        let mut deferred_error = None;
        let mut cancelled = false;
        let scan_result = scanner.scan_chunk(&chunk[..read], absolute_start, |absolute_end| {
            let local_end = (absolute_end - absolute_start) as usize;
            if found.is_none() && deferred_error.is_none() && !cancelled {
                if shared.cancelled.load(Ordering::Acquire) {
                    cancelled = true;
                } else if row_number >= start.row {
                    record.extend_from_slice(&chunk[segment_start..local_end]);
                    if record.len() > max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: max_record_bytes,
                        });
                    } else {
                        match find_record(
                            &finder,
                            &record,
                            delimiter,
                            row_number,
                            record_start,
                            start,
                        ) {
                            Ok(result) => found = result,
                            Err(error) => deferred_error = Some(error),
                        }
                    }
                }
            }
            record.clear();
            row_number += 1;
            record_start = absolute_end;
            records_scanned += 1;
            segment_start = local_end;
        });

        absolute_start += read as u64;
        shared
            .bytes_scanned
            .store(absolute_start - checkpoint.offset, Ordering::Release);
        shared
            .rows_scanned
            .store(records_scanned, Ordering::Release);
        if let Some(found) = found {
            return Ok(SearchOutcome::Match(found));
        }
        if cancelled || shared.cancelled.load(Ordering::Acquire) {
            return Ok(SearchOutcome::Cancelled);
        }
        if let Some(error) = deferred_error {
            return Err(error);
        }
        scan_result?;

        if row_number >= start.row {
            record.extend_from_slice(&chunk[segment_start..read]);
            if record.len() > max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: max_record_bytes,
                });
            }
        }
    }
}

fn find_record(
    finder: &Finder<'_>,
    record: &[u8],
    delimiter: u8,
    row: u64,
    record_offset: u64,
    start: SearchPosition,
) -> Result<Option<SearchMatch>, QuarryError> {
    let first_column = if row == start.row { start.column } else { 0 };
    let fields = parse_source_record(record, delimiter, row)?;
    Ok(fields
        .iter()
        .enumerate()
        .skip(first_column)
        .find(|(_, field)| finder.find(field.as_ref()).is_some())
        .map(|(column, _)| SearchMatch {
            row,
            column,
            record_offset,
        }))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::{
        SearchJob, SearchOutcome, SearchPosition, SharedState, WorkerCompletion, run_search,
    };
    use crate::{
        Checkpoint, DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, HeaderMode, IndexConfig,
        OpenOptions, QuarryError, Session,
    };

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture(bytes: &[u8]) -> std::path::PathBuf {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("quarry-search-{}-{id}.csv", std::process::id()));
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        path
    }

    fn session_and_index(
        path: &std::path::Path,
        has_header: bool,
    ) -> (Session, crate::StructuralIndex) {
        let session = Session::open(
            path,
            OpenOptions {
                header_mode: if has_header {
                    HeaderMode::FirstRow
                } else {
                    HeaderMode::NoHeader
                },
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let index = session
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        (session, index)
    }

    #[test]
    fn finds_decoded_quoted_multiline_cell_across_chunks() {
        let mut bytes = b"name,value\n\"".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', DEFAULT_READ_CHUNK));
        bytes.extend_from_slice(b"\ntarget \"\"quoted\"\"\",tail\n");
        let expected_offset = b"name,value\n".len() as u64;
        let path = fixture(&bytes);
        let (session, index) = session_and_index(&path, true);

        let outcome = session
            .start_search(
                &index,
                b"\ntarget \"quoted\"".to_vec(),
                SearchPosition { row: 1, column: 0 },
            )
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(
            outcome,
            SearchOutcome::Match(super::SearchMatch {
                row: 1,
                column: 0,
                record_offset: expected_offset,
            })
        );
        assert_eq!(
            session
                .start_search(
                    &index,
                    b"\"\"quoted\"\"".to_vec(),
                    SearchPosition { row: 1, column: 0 },
                )
                .unwrap()
                .wait()
                .unwrap(),
            SearchOutcome::NotFound
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_is_case_sensitive_honors_cursor_and_completes_exactly() {
        let path = fixture(b"needle,header\nNeedle,needle\nneedle,tail\n");
        let (session, index) = session_and_index(&path, true);

        let first = session
            .start_search(
                &index,
                b"needle".to_vec(),
                SearchPosition { row: 0, column: 1 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(
            first,
            SearchOutcome::Match(super::SearchMatch {
                row: 1,
                column: 1,
                ..
            })
        ));

        let second = session
            .start_search(
                &index,
                b"needle".to_vec(),
                SearchPosition { row: 1, column: 2 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(
            second,
            SearchOutcome::Match(super::SearchMatch {
                row: 2,
                column: 0,
                ..
            })
        ));

        let job = session
            .start_search(
                &index,
                b"missing".to_vec(),
                SearchPosition { row: 2, column: 1 },
            )
            .unwrap();
        while !job.progress().done {
            thread::yield_now();
        }
        let progress = job.progress();
        assert_eq!(progress.bytes_scanned, progress.total_bytes);
        assert!(progress.rows_scanned > 0);
        assert!(progress.elapsed > Duration::ZERO);
        assert!(!progress.cancelled);
        assert_eq!(job.wait().unwrap(), SearchOutcome::NotFound);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn searches_unterminated_final_record_from_a_checkpoint() {
        let path = fixture(b"a,b\r\nc,target");
        let session = Session::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let index = session
            .start_indexing(IndexConfig {
                chunk_bytes: 2,
                checkpoint_every: 1,
                memory_budget_bytes: 64,
            })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            index.nearest_checkpoint(1),
            Checkpoint { row: 1, offset: 5 }
        );

        let outcome = session
            .start_search(
                &index,
                b"target".to_vec(),
                SearchPosition { row: 1, column: 0 },
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            outcome,
            SearchOutcome::Match(super::SearchMatch {
                row: 1,
                column: 1,
                record_offset: 5,
            })
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn search_honors_cancellation_before_reading() {
        let path = fixture(b"a,b\n");
        let file = File::open(&path).unwrap();
        let shared = SharedState::new(fs::metadata(&path).unwrap().len());
        shared.cancelled.store(true, Ordering::Release);

        let outcome = run_search(
            file,
            b',',
            b"missing",
            SearchPosition { row: 0, column: 0 },
            Checkpoint { row: 0, offset: 0 },
            DEFAULT_MAX_RECORD_BYTES,
            &shared,
        )
        .unwrap();

        assert_eq!(outcome, SearchOutcome::Cancelled);
        assert_eq!(shared.bytes_scanned.load(Ordering::Acquire), 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn dropping_an_active_search_cancels_and_joins_its_worker() {
        let shared = Arc::new(SharedState::new(0));
        let worker_state = Arc::clone(&shared);
        let exited = Arc::new(AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let handle = thread::spawn(move || {
            let _completion = WorkerCompletion(&worker_state);
            while !worker_state.cancelled.load(Ordering::Acquire) {
                thread::yield_now();
            }
            worker_exited.store(true, Ordering::Release);
            Ok(SearchOutcome::Cancelled)
        });
        let job = SearchJob {
            shared: Arc::clone(&shared),
            handle: Some(handle),
        };

        drop(job);

        assert!(shared.cancelled.load(Ordering::Acquire));
        assert!(shared.done.load(Ordering::Acquire));
        assert!(exited.load(Ordering::Acquire));
    }

    #[test]
    fn search_rejects_records_over_the_bounded_cap() {
        let path = fixture(b"123456789,tail\n");
        let (session, index) = session_and_index(&path, false);
        let checkpoint = index.nearest_checkpoint(0);
        let job = SearchJob::start(
            session.path().to_path_buf(),
            session.file_size,
            session.dialect.delimiter,
            b"missing".to_vec(),
            SearchPosition { row: 0, column: 0 },
            checkpoint,
            8,
        )
        .unwrap();
        while !job.progress().done {
            thread::yield_now();
        }
        assert!(job.error().unwrap().contains("8-byte limit"));
        assert!(matches!(
            job.wait(),
            Err(QuarryError::RecordTooLarge { limit: 8 })
        ));
        assert_eq!(DEFAULT_MAX_RECORD_BYTES, 64 * 1024 * 1024);

        fs::remove_file(path).unwrap();
    }
}
