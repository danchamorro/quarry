//! Duplicate analysis reuses the external-sort workspace and guarded export.
//! The completed candidate is private until the caller explicitly accepts it.

use super::*;

/// Match selected decoded fields, treating missing fields as empty. Insensitive
/// comparison folds ASCII letters only; whitespace and non-ASCII bytes are exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateSpec {
    pub columns: Vec<usize>,
    pub case_sensitivity: CaseSensitivity,
}

impl DuplicateSpec {
    fn validate(&self, max_key_bytes: usize) -> Result<(), QuarryError> {
        if self.columns.is_empty() {
            return Err(QuarryError::InvalidOption(
                "choose at least one column for duplicate matching",
            ));
        }
        let mut columns = self.columns.clone();
        columns.sort_unstable();
        if columns.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(QuarryError::InvalidOption(
                "duplicate matching columns must be distinct",
            ));
        }
        if self.columns.len() > max_key_bytes / 8 {
            return Err(QuarryError::RecordTooLarge {
                limit: max_key_bytes,
            });
        }
        Ok(())
    }

    pub(super) fn key<'a>(
        &self,
        value_at: impl Fn(usize) -> &'a [u8],
        max_key_bytes: usize,
    ) -> Result<Vec<u8>, QuarryError> {
        let mut key = Vec::new();
        for &column in &self.columns {
            let value = value_at(column);
            if key.len().saturating_add(8).saturating_add(value.len()) > max_key_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: max_key_bytes,
                });
            }
            // Length framing distinguishes ("ab", "c") from ("a", "bc"),
            // including embedded NULs and arbitrary bytes in decoded fields.
            key.extend_from_slice(&(value.len() as u64).to_le_bytes());
            if self.case_sensitivity == CaseSensitivity::Insensitive {
                key.extend(value.iter().map(u8::to_ascii_lowercase));
            } else {
                key.extend_from_slice(value);
            }
        }
        Ok(key)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DuplicateProgress {
    pub bytes_scanned: u64,
    pub total_bytes: u64,
    pub rows_scanned: u64,
    pub duplicate_rows: u64,
    pub retained_rows: u64,
    pub bytes_written: u64,
    pub runs_created: u64,
    pub peak_temporary_bytes: u64,
    pub merge_passes: u64,
    pub header_rows: u64,
    pub elapsed: Duration,
    pub cancellation_latency: Option<Duration>,
    pub done: bool,
    pub cancelled: bool,
}

/// A private, complete candidate. The caller owns its lifetime and must delete
/// it when the preview is dismissed, or adopt it only after explicit removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateSummary {
    pub destination: PathBuf,
    pub rows_scanned: u64,
    pub duplicate_rows: u64,
    pub retained_rows: u64,
    pub bytes_written: u64,
    pub runs_created: u64,
    pub peak_temporary_bytes: u64,
    pub merge_passes: u64,
    pub header_rows: u64,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplicateOutcome {
    Complete(DuplicateSummary),
    Cancelled,
}

struct DuplicateState {
    sort: SharedState,
    duplicate_rows: AtomicU64,
    retained_rows: AtomicU64,
}

pub struct DuplicateJob {
    shared: Arc<DuplicateState>,
    handle: Option<JoinHandle<Result<DuplicateOutcome, QuarryError>>>,
}

impl DuplicateJob {
    #[allow(clippy::too_many_arguments)]
    fn start(
        session: &Session,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        spec: DuplicateSpec,
        destination: PathBuf,
        config: SortConfig,
    ) -> Result<Self, QuarryError> {
        validate_config(config)?;
        validate_overlays(
            session.dialect.has_header,
            &header_renames,
            &cell_edits,
            config,
        )?;
        spec.validate(config.max_record_bytes)?;
        let source_path = session.path.clone();
        let source_stamp = session.source_stamp.clone();
        let delimiter = session.dialect.delimiter;
        let has_header = session.dialect.has_header;
        let mut source = File::open(&source_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
        if !source_matches_stamp(&source, &source_path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        let bom_present = source_has_bom(&mut source)?;
        let output = ExportTarget::new_private_guarded(
            &source_path,
            destination.clone(),
            &source,
            source_stamp.clone(),
        )?;
        let shared = Arc::new(DuplicateState {
            sort: SharedState::new(session.file_size),
            duplicate_rows: AtomicU64::new(0),
            retained_rows: AtomicU64::new(0),
        });
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-duplicates".into())
            .spawn(move || {
                let mut completion = WorkerCompletion::new(&worker_state.sort);
                let mut result = run_duplicates(
                    &mut source,
                    &source_path,
                    &source_stamp,
                    delimiter,
                    has_header,
                    &header_renames,
                    &cell_edits,
                    &spec,
                    destination,
                    output,
                    bom_present,
                    config,
                    &worker_state,
                );
                match &result {
                    Ok(DuplicateOutcome::Cancelled) => {
                        worker_state.sort.cancelled.store(true, Ordering::Release);
                    }
                    Err(error) => {
                        *worker_state.sort.error.lock().unwrap() = Some(error.to_string());
                    }
                    Ok(DuplicateOutcome::Complete(_)) => {}
                }
                let elapsed = completion.finish();
                if let Ok(DuplicateOutcome::Complete(summary)) = &mut result {
                    summary.elapsed = elapsed;
                }
                result
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> DuplicateProgress {
        let shared = &self.shared.sort;
        DuplicateProgress {
            bytes_scanned: shared.bytes_scanned.load(Ordering::Acquire),
            total_bytes: shared.total_bytes,
            rows_scanned: shared.rows_sorted.load(Ordering::Acquire),
            duplicate_rows: self.shared.duplicate_rows.load(Ordering::Acquire),
            retained_rows: self.shared.retained_rows.load(Ordering::Acquire),
            bytes_written: shared.bytes_written.load(Ordering::Acquire),
            runs_created: shared.runs_created.load(Ordering::Acquire),
            peak_temporary_bytes: shared.peak_temporary_bytes.load(Ordering::Acquire),
            merge_passes: shared.merge_passes.load(Ordering::Acquire),
            header_rows: shared.header_rows.load(Ordering::Acquire),
            elapsed: shared.elapsed(),
            cancellation_latency: shared.cancellation_latency(),
            done: shared.done.load(Ordering::Acquire),
            cancelled: shared.cancelled.load(Ordering::Acquire),
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.sort.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.shared.sort.request_cancel();
    }

    pub fn wait(mut self) -> Result<DuplicateOutcome, QuarryError> {
        self.handle
            .take()
            .expect("duplicate handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl Drop for DuplicateJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.shared.sort.request_cancel();
            // A worker can finish just before cancellation. Never strand its
            // completed candidate when the caller abandons the job.
            if let Ok(Ok(DuplicateOutcome::Complete(summary))) = handle.join() {
                let _ = fs::remove_file(summary.destination);
            }
        }
    }
}

impl Session {
    /// Analyze duplicates into a private candidate in retained source order.
    /// The destination must not exist. Unsaved edits participate in matching.
    /// Accept the candidate only after showing its duplicate count to the user.
    pub fn start_find_duplicates(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        spec: DuplicateSpec,
        destination: impl AsRef<Path>,
    ) -> Result<DuplicateJob, QuarryError> {
        DuplicateJob::start(
            self,
            header_renames,
            cell_edits,
            spec,
            destination.as_ref().to_path_buf(),
            DEFAULT_SORT_CONFIG,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn run_duplicates(
    source: &mut File,
    source_path: &Path,
    source_stamp: &SourceStamp,
    delimiter: u8,
    has_header: bool,
    header_renames: &BTreeMap<usize, Vec<u8>>,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    spec: &DuplicateSpec,
    destination: PathBuf,
    mut output: ExportTarget,
    bom_present: bool,
    config: SortConfig,
    state: &DuplicateState,
) -> Result<DuplicateOutcome, QuarryError> {
    let shared = &state.sort;
    let workspace = RunWorkspace::create(&destination)?;
    let built = (|| {
        let ScanOutcome::Complete(mut scan) = create_initial_runs(
            source,
            delimiter,
            has_header,
            header_renames,
            cell_edits,
            spec,
            bom_present,
            config,
            &workspace,
            shared,
        )?
        else {
            return Ok(None);
        };
        if !source_matches_stamp(source, source_path, source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        if !reduce_runs(
            &mut scan.runs,
            SortDirection::Ascending,
            scan.max_key_bytes,
            config,
            &workspace,
            shared,
        )? {
            return Ok(None);
        }
        let Some((mut survivors, records)) =
            retain_first_runs(&scan.runs, config, &workspace, state)?
        else {
            return Ok(None);
        };
        if state.retained_rows.load(Ordering::Acquire)
            + state.duplicate_rows.load(Ordering::Acquire)
            != scan.rows
        {
            return Err(invalid_sort_output("duplicate run row count changed"));
        }
        remove_runs(&scan.runs, shared)?;
        if !reduce_runs(
            &mut survivors,
            SortDirection::Ascending,
            8,
            config,
            &workspace,
            shared,
        )? {
            return Ok(None);
        }
        let mut prefix_bytes = 0;
        if bom_present {
            write_output(&mut output, UTF8_BOM, shared)?;
            prefix_bytes += UTF8_BOM.len() as u64;
        }
        if let Some(header) = &scan.header {
            write_output(&mut output, header, shared)?;
            prefix_bytes += header.len() as u64;
        }
        let Some((retained, data_bytes, _)) = merge_runs_to_output(
            &survivors,
            &mut output,
            SortDirection::Ascending,
            &scan.preferred_ending,
            records,
            config.max_record_bytes,
            prefix_bytes,
            shared,
        )?
        else {
            return Ok(None);
        };
        if retained != state.retained_rows.load(Ordering::Acquire) {
            return Err(invalid_sort_output("retained duplicate row count changed"));
        }
        if !survivors.is_empty() {
            shared.merge_passes.fetch_add(1, Ordering::AcqRel);
        }
        let bytes_written = prefix_bytes.saturating_add(data_bytes);
        shared.bytes_written.store(bytes_written, Ordering::Release);
        Ok(Some((scan.rows, retained, bytes_written)))
    })();
    let cleanup = workspace.cleanup();
    let built = built?;
    cleanup?;
    let Some((rows_scanned, retained_rows, bytes_written)) = built else {
        return Ok(DuplicateOutcome::Cancelled);
    };
    match output.publish(retained_rows, bytes_written, &shared.cancel_requested)? {
        FilterExportOutcome::Cancelled => Ok(DuplicateOutcome::Cancelled),
        FilterExportOutcome::Complete(summary) => {
            Ok(DuplicateOutcome::Complete(DuplicateSummary {
                destination: summary.destination,
                rows_scanned,
                duplicate_rows: rows_scanned - retained_rows,
                retained_rows,
                bytes_written: summary.bytes_written,
                runs_created: shared.runs_created.load(Ordering::Acquire),
                peak_temporary_bytes: shared.peak_temporary_bytes.load(Ordering::Acquire),
                merge_passes: shared.merge_passes.load(Ordering::Acquire),
                header_rows: shared.header_rows.load(Ordering::Acquire),
                elapsed: Duration::ZERO,
            }))
        }
    }
}

fn remove_runs(paths: &[PathBuf], shared: &SharedState) -> Result<(), QuarryError> {
    for path in paths {
        let bytes = fs::metadata(path)?.len();
        fs::remove_file(path)?;
        shared.remove_temporary_bytes(bytes);
    }
    Ok(())
}

fn flush_survivors(
    entries: &mut Vec<RunEntry>,
    runs: &mut Vec<PathBuf>,
    workspace: &RunWorkspace,
    shared: &SharedState,
) -> Result<bool, QuarryError> {
    if entries.is_empty() {
        return Ok(true);
    }
    entries.sort_unstable_by_key(|entry| entry.ordinal);
    let (path, mut writer) = workspace.create_run()?;
    for entry in entries.drain(..) {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(false);
        }
        shared.add_temporary_bytes(write_entry(&mut writer, &entry)?);
    }
    writer.flush()?;
    runs.push(path);
    shared.runs_created.fetch_add(1, Ordering::AcqRel);
    Ok(true)
}

fn retain_first_runs(
    paths: &[PathBuf],
    config: SortConfig,
    workspace: &RunWorkspace,
    state: &DuplicateState,
) -> Result<Option<(Vec<PathBuf>, RecordMultisetFingerprint)>, QuarryError> {
    let shared = &state.sort;
    let mut readers = open_run_readers(paths)?;
    let Some(mut heap) = seed_heap(
        &mut readers,
        SortDirection::Ascending,
        config.max_record_bytes,
        shared,
    )?
    else {
        return Ok(None);
    };
    let mut previous_key: Option<Vec<u8>> = None;
    let mut entries = Vec::new();
    let mut entry_bytes = 0_usize;
    let mut runs = Vec::new();
    let mut records = RecordMultisetFingerprint::default();
    while let Some(item) = heap.pop() {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        let reader = &mut readers[item.run_index];
        if previous_key.as_ref() == Some(&item.head.key) {
            reader.seek_relative(item.head.record_len as i64)?;
            state.duplicate_rows.fetch_add(1, Ordering::AcqRel);
        } else {
            let record = read_record(reader, item.head.record_len)?;
            let entry_size = record.len().saturating_add(32);
            if !entries.is_empty()
                && entry_bytes.saturating_add(entry_size) > config.run_memory_bytes
            {
                if !flush_survivors(&mut entries, &mut runs, workspace, shared)? {
                    return Ok(None);
                }
                entry_bytes = 0;
            }
            records.observe(&record);
            entries.push(RunEntry {
                key: item.head.ordinal.to_be_bytes().to_vec(),
                record,
                ordinal: item.head.ordinal,
            });
            entry_bytes = entry_bytes.saturating_add(entry_size);
            previous_key = Some(item.head.key);
            state.retained_rows.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(head) = read_head(reader, config.max_record_bytes)? {
            heap.push(HeapEntry {
                head,
                run_index: item.run_index,
                direction: SortDirection::Ascending,
            });
        }
    }
    if !flush_survivors(&mut entries, &mut runs, workspace, shared)? {
        return Ok(None);
    }
    if !paths.is_empty() {
        shared.merge_passes.fetch_add(1, Ordering::AcqRel);
    }
    Ok(Some((runs, records)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HeaderMode, OpenOptions as QuarryOpenOptions};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    static NEXT_CASE: AtomicU64 = AtomicU64::new(0);

    struct Case {
        directory: PathBuf,
        source: PathBuf,
        destination: PathBuf,
    }

    impl Case {
        fn new(bytes: &[u8]) -> Self {
            let id = NEXT_CASE.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "quarry-duplicates-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&directory).unwrap();
            let source = directory.join("source.csv");
            fs::write(&source, bytes).unwrap();
            let destination = directory.join("candidate.csv");
            Self {
                directory,
                source,
                destination,
            }
        }

        fn session(&self, header_mode: HeaderMode) -> Session {
            Session::open(
                &self.source,
                QuarryOpenOptions {
                    rows: 1,
                    delimiter: Some(b','),
                    header_mode,
                    sample_bytes: 32,
                    bootstrap_limit: 64 * 1024 * 1024,
                },
            )
            .unwrap()
        }

        fn artifacts(&self) -> Vec<PathBuf> {
            fs::read_dir(&self.directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".quarry-")
                })
                .collect()
        }
    }

    impl Drop for Case {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn spec(columns: &[usize], case_sensitivity: CaseSensitivity) -> DuplicateSpec {
        DuplicateSpec {
            columns: columns.to_vec(),
            case_sensitivity,
        }
    }

    fn tiny_config() -> SortConfig {
        SortConfig {
            chunk_bytes: 7,
            max_record_bytes: 1024,
            run_memory_bytes: 96,
            merge_fan_in: 2,
        }
    }

    fn wait_done(job: &DuplicateJob) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !job.progress().done {
            assert!(Instant::now() < deadline, "duplicate worker did not finish");
            thread::yield_now();
        }
    }

    #[test]
    fn decoded_selected_fields_preserve_exact_first_records_and_case_semantics() {
        let records: &[&[u8]] = &[
            b"B,one,first\r\n",
            b"\"a\",\"multi\nline\",\"quote \"\"kept\"\"\"\r\n",
            b"b,one,later\r\n",
            b"A,\"multi\nline\",later\r\n",
            b"blank\r\n",
            b"blank,,later\r\n",
            b"ab,c,boundary\r\n",
            b"a,bc,boundary\r\n",
            b"\xC3\x84,x,upper\r\n",
            b"\xC3\xA4,x,lower\r\n",
            b"B,two,different\r\n",
            b"B,two,different",
        ];
        let mut bytes = b"\xEF\xBB\xBFkey,value,note\r\n".to_vec();
        for record in records {
            bytes.extend_from_slice(record);
        }
        for (case, retained) in [
            (CaseSensitivity::Insensitive, vec![0, 1, 4, 6, 7, 8, 9, 10]),
            (
                CaseSensitivity::Sensitive,
                vec![0, 1, 2, 3, 4, 6, 7, 8, 9, 10],
            ),
        ] {
            let fixture = Case::new(&bytes);
            let job = DuplicateJob::start(
                &fixture.session(HeaderMode::FirstRow),
                BTreeMap::new(),
                BTreeMap::new(),
                spec(&[0, 1], case),
                fixture.destination.clone(),
                tiny_config(),
            )
            .unwrap();
            wait_done(&job);
            let progress = job.progress();
            let DuplicateOutcome::Complete(summary) = job.wait().unwrap() else {
                panic!("cancelled");
            };
            let mut expected = b"\xEF\xBB\xBFkey,value,note\r\n".to_vec();
            for &index in &retained {
                expected.extend_from_slice(records[index]);
            }
            assert_eq!(fs::read(&fixture.destination).unwrap(), expected);
            assert_eq!(fs::read(&fixture.source).unwrap(), bytes);
            assert_eq!(summary.rows_scanned, records.len() as u64);
            assert_eq!(summary.retained_rows, retained.len() as u64);
            assert_eq!(
                summary.duplicate_rows,
                (records.len() - retained.len()) as u64
            );
            assert_eq!(summary.bytes_written, expected.len() as u64);
            assert_eq!(progress.rows_scanned, summary.rows_scanned);
            assert_eq!(progress.duplicate_rows, summary.duplicate_rows);
            assert_eq!(progress.retained_rows, summary.retained_rows);
            assert_eq!(progress.bytes_scanned, bytes.len() as u64);
            assert_eq!(progress.bytes_written, summary.bytes_written);
            assert_eq!(progress.elapsed, summary.elapsed);
            assert!(summary.merge_passes >= 3);
            assert_eq!(summary.header_rows, 1);
            assert!(fixture.artifacts().is_empty());
            #[cfg(unix)]
            assert_eq!(
                fs::metadata(&fixture.destination)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn current_cell_and_header_edits_determine_duplicates_and_output() {
        let bytes = b"\xEF\xBB\xBFkey,note\r\nB,first\r\na,second\r\nb,third\r\nZ,last";
        let fixture = Case::new(bytes);
        let job = DuplicateJob::start(
            &fixture.session(HeaderMode::FirstRow),
            BTreeMap::from([(0, b"KEY".to_vec())]),
            BTreeMap::from([
                ((1, 0), b"a".to_vec()),
                ((1, 1), b"edited,first".to_vec()),
                ((4, 1), b"last\nline".to_vec()),
            ]),
            spec(&[0], CaseSensitivity::Insensitive),
            fixture.destination.clone(),
            tiny_config(),
        )
        .unwrap();
        let DuplicateOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("cancelled");
        };
        assert_eq!(summary.duplicate_rows, 1);
        assert_eq!(
            fs::read(&fixture.destination).unwrap(),
            b"\xEF\xBB\xBFKEY,note\r\na,\"edited,first\"\r\nb,third\r\nZ,\"last\nline\""
        );
        assert_eq!(fs::read(&fixture.source).unwrap(), bytes);
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn headerless_bom_empty_files_missing_fields_and_last_ending_are_preserved() {
        for (bytes, expected, header_mode, duplicates) in [
            (
                b"\xEF\xBB\xBFz,first\n\xEF\xBB\xBFa,second\na,last".as_slice(),
                b"\xEF\xBB\xBFz,first\n\xEF\xBB\xBFa,second\na,last".as_slice(),
                HeaderMode::NoHeader,
                0,
            ),
            (
                b"\xEF\xBB\xBFx,first\nx,later\ny,last".as_slice(),
                b"\xEF\xBB\xBFx,first\ny,last".as_slice(),
                HeaderMode::NoHeader,
                1,
            ),
            (b"".as_slice(), b"".as_slice(), HeaderMode::NoHeader, 0),
            (
                b"\xEF\xBB\xBFkey\r\n".as_slice(),
                b"\xEF\xBB\xBFkey\r\n".as_slice(),
                HeaderMode::FirstRow,
                0,
            ),
            (
                b"\nx\n".as_slice(),
                b"\nx\n".as_slice(),
                HeaderMode::NoHeader,
                0,
            ),
        ] {
            let fixture = Case::new(bytes);
            let job = fixture
                .session(header_mode)
                .start_find_duplicates(
                    BTreeMap::new(),
                    BTreeMap::new(),
                    spec(&[0], CaseSensitivity::Sensitive),
                    &fixture.destination,
                )
                .unwrap();
            let DuplicateOutcome::Complete(summary) = job.wait().unwrap() else {
                panic!("cancelled");
            };
            assert_eq!(summary.duplicate_rows, duplicates);
            assert_eq!(fs::read(&fixture.destination).unwrap(), expected);
            assert_eq!(fs::read(&fixture.source).unwrap(), bytes);
            assert!(fixture.artifacts().is_empty());
        }
        let fixture = Case::new(b"a,first\nb,second\n");
        let job = fixture
            .session(HeaderMode::NoHeader)
            .start_find_duplicates(
                BTreeMap::new(),
                BTreeMap::new(),
                spec(&[usize::MAX], CaseSensitivity::Sensitive),
                &fixture.destination,
            )
            .unwrap();
        let DuplicateOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("cancelled");
        };
        assert_eq!(summary.duplicate_rows, 1);
        assert_eq!(fs::read(&fixture.destination).unwrap(), b"a,first\n");
    }

    #[test]
    fn multiple_merge_generations_restore_source_order() {
        let mut bytes = b"key,id\n".to_vec();
        let mut expected = bytes.clone();
        for row in 0..512 {
            let record = format!("{},{row}\n", (511 - row) % 73);
            bytes.extend_from_slice(record.as_bytes());
            if row < 73 {
                expected.extend_from_slice(record.as_bytes());
            }
        }
        let fixture = Case::new(&bytes);
        let job = DuplicateJob::start(
            &fixture.session(HeaderMode::FirstRow),
            BTreeMap::new(),
            BTreeMap::new(),
            spec(&[0], CaseSensitivity::Sensitive),
            fixture.destination.clone(),
            tiny_config(),
        )
        .unwrap();
        let DuplicateOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("cancelled");
        };
        assert_eq!(summary.retained_rows, 73);
        assert_eq!(summary.duplicate_rows, 439);
        assert!(summary.merge_passes > 6);
        assert!(summary.runs_created > 100);
        assert_eq!(fs::read(&fixture.destination).unwrap(), expected);
        assert_eq!(fs::read(&fixture.source).unwrap(), bytes);
        assert!(fixture.artifacts().is_empty());
    }

    #[test]
    fn invalid_specs_keys_and_edits_fail_without_publishing() {
        let fixture = Case::new(b"key,note\na,first\na,later\n");
        let session = fixture.session(HeaderMode::FirstRow);
        for columns in [vec![], vec![0, 0]] {
            assert!(matches!(
                session.start_find_duplicates(
                    BTreeMap::new(),
                    BTreeMap::new(),
                    DuplicateSpec {
                        columns,
                        case_sensitivity: CaseSensitivity::Sensitive
                    },
                    &fixture.destination
                ),
                Err(QuarryError::InvalidOption(_))
            ));
        }
        for edits in [
            BTreeMap::from([((0, 0), b"header".to_vec())]),
            BTreeMap::from([((1, 2), b"missing column".to_vec())]),
            BTreeMap::from([((3, 0), b"missing row".to_vec())]),
        ] {
            let result = session
                .start_find_duplicates(
                    BTreeMap::new(),
                    edits,
                    spec(&[0], CaseSensitivity::Sensitive),
                    &fixture.destination,
                )
                .and_then(DuplicateJob::wait);
            assert!(matches!(result, Err(QuarryError::InvalidOption(_))));
            assert!(!fixture.destination.exists());
            assert!(fixture.artifacts().is_empty());
        }
        let wide = Case::new(b"abcdefghijklmnopqrstuvwxy,1\n");
        let job = DuplicateJob::start(
            &wide.session(HeaderMode::NoHeader),
            BTreeMap::new(),
            BTreeMap::new(),
            spec(&[0], CaseSensitivity::Sensitive),
            wide.destination.clone(),
            SortConfig {
                max_record_bytes: 32,
                ..tiny_config()
            },
        )
        .unwrap();
        assert!(matches!(
            job.wait(),
            Err(QuarryError::RecordTooLarge { limit: 32 })
        ));
        assert!(!wide.destination.exists());
        assert!(wide.artifacts().is_empty());
        let fields = [b"a\0".as_slice(), b"b".as_slice()];
        let other = [b"a".as_slice(), b"\0b".as_slice()];
        let spec = spec(&[0, 1], CaseSensitivity::Sensitive);
        assert_ne!(
            spec.key(|column| fields[column], 64).unwrap(),
            spec.key(|column| other[column], 64).unwrap()
        );
    }

    #[test]
    fn cancellation_and_abandoned_completed_jobs_remove_every_temporary_file() {
        let bytes = b"key,note\na,same\n".repeat(10_000);
        let fixture = Case::new(&bytes);
        let job = DuplicateJob::start(
            &fixture.session(HeaderMode::FirstRow),
            BTreeMap::new(),
            BTreeMap::new(),
            spec(&[0], CaseSensitivity::Sensitive),
            fixture.destination.clone(),
            SortConfig {
                chunk_bytes: 1,
                ..tiny_config()
            },
        )
        .unwrap();
        job.cancel();
        wait_done(&job);
        assert!(job.progress().cancelled);
        assert!(job.progress().cancellation_latency.is_some());
        assert_eq!(job.wait().unwrap(), DuplicateOutcome::Cancelled);
        assert!(!fixture.destination.exists());
        assert!(fixture.artifacts().is_empty());
        assert_eq!(fs::read(&fixture.source).unwrap(), bytes);

        let small = Case::new(b"key\na\na\n");
        let job = small
            .session(HeaderMode::FirstRow)
            .start_find_duplicates(
                BTreeMap::new(),
                BTreeMap::new(),
                spec(&[0], CaseSensitivity::Sensitive),
                &small.destination,
            )
            .unwrap();
        wait_done(&job);
        assert!(small.destination.exists());
        drop(job);
        assert!(!small.destination.exists());
        assert!(small.artifacts().is_empty());
    }

    #[test]
    fn source_replacement_and_destination_conflicts_never_publish() {
        let bytes = b"key,note\na,first\na,later\n";
        let fixture = Case::new(bytes);
        let session = fixture.session(HeaderMode::FirstRow);
        let mut source = File::open(&fixture.source).unwrap();
        let output = ExportTarget::new_private_guarded(
            &fixture.source,
            fixture.destination.clone(),
            &source,
            session.source_stamp.clone(),
        )
        .unwrap();
        let moved = fixture.directory.join("original.csv");
        fs::rename(&fixture.source, &moved).unwrap();
        fs::write(&fixture.source, b"external,change\n").unwrap();
        let state = DuplicateState {
            sort: SharedState::new(bytes.len() as u64),
            duplicate_rows: AtomicU64::new(0),
            retained_rows: AtomicU64::new(0),
        };
        let result = run_duplicates(
            &mut source,
            &fixture.source,
            &session.source_stamp,
            b',',
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &spec(&[0], CaseSensitivity::Sensitive),
            fixture.destination.clone(),
            output,
            false,
            tiny_config(),
            &state,
        );
        assert!(matches!(result, Err(QuarryError::SourceChanged)));
        assert!(!fixture.destination.exists());
        assert_eq!(fs::read(&fixture.source).unwrap(), b"external,change\n");
        assert_eq!(fs::read(moved).unwrap(), bytes);
        assert!(fixture.artifacts().is_empty());

        let session = fixture.session(HeaderMode::FirstRow);
        fs::write(&fixture.destination, b"existing\n").unwrap();
        assert!(matches!(
            session.start_find_duplicates(
                BTreeMap::new(),
                BTreeMap::new(),
                spec(&[0], CaseSensitivity::Sensitive),
                &fixture.destination
            ),
            Err(QuarryError::ExportDestinationExists)
        ));
        assert_eq!(fs::read(&fixture.destination).unwrap(), b"existing\n");
        assert!(matches!(
            session.start_find_duplicates(
                BTreeMap::new(),
                BTreeMap::new(),
                spec(&[0], CaseSensitivity::Sensitive),
                &fixture.source
            ),
            Err(QuarryError::ExportDestinationIsSource)
        ));
    }
}
