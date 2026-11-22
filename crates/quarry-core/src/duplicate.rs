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

/// Execution options for duplicate removal, including parallel striping, scratch location,
/// and the adaptive single-run memory budget (300 MB default, capped by physical RAM).
#[derive(Debug, Clone, Default)]
pub struct DuplicateOptions<'a> {
    pub temporary_directory: Option<&'a Path>,
    pub source_index: Option<&'a StructuralIndex>,
    pub threads: Option<usize>,
    pub memory_budget_bytes: Option<usize>,
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
    pub effective_workers: usize,
    pub elapsed: Duration,
    pub cancellation_latency: Option<Duration>,
    /// Ready to consume with wait(), not proof of publication. The candidate
    /// remains staged until wait() accepts it and the final guards pass.
    pub done: bool,
    pub cancelled: bool,
}

/// A private, complete candidate. The caller owns its lifetime and must delete
/// it when the preview is dismissed, or adopt it only after explicit removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateSummary {
    pub destination: PathBuf,
    pub output_index: Option<CompletedIndex>,
    pub rows_scanned: u64,
    pub duplicate_rows: u64,
    pub retained_rows: u64,
    pub bytes_written: u64,
    pub runs_created: u64,
    pub peak_temporary_bytes: u64,
    pub merge_passes: u64,
    pub header_rows: u64,
    pub effective_workers: usize,
    pub in_memory_fast_path: bool,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplicateOutcome {
    Complete(DuplicateSummary),
    Cancelled,
}

struct DuplicateState {
    sort: Arc<SharedState>,
    duplicate_rows: Arc<AtomicU64>,
    retained_rows: AtomicU64,
}

pub struct DuplicateJob {
    publication: Arc<PrivatePublication>,
    shared: Arc<DuplicateState>,
    handle: Option<JoinHandle<Result<DuplicateOutcome, QuarryError>>>,
}

impl DuplicateJob {
    /// Retain the private generation owner before accepting publication.
    pub fn retain_working_storage(
        &self,
        storage: Arc<crate::WorkingCopyStorage>,
    ) -> Result<(), QuarryError> {
        self.publication.retain_storage(storage)
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn start(
        session: &Session,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        spec: DuplicateSpec,
        destination: PathBuf,
        config: SortConfig,
    ) -> Result<Self, QuarryError> {
        Self::start_with_options_internal(
            session,
            header_renames,
            cell_edits,
            spec,
            destination,
            config,
            None,
            session.completed_record_count(),
            None,
            Some(1),
            parallel::DEFAULT_MIN_PARALLEL_STRIPE_BYTES,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_options_internal(
        session: &Session,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        spec: DuplicateSpec,
        destination: PathBuf,
        config: SortConfig,
        temporary_directory: Option<PathBuf>,
        completed_record_count: Option<u64>,
        source_index: Option<StructuralIndex>,
        threads: Option<usize>,
        min_parallel_stripe_bytes: u64,
    ) -> Result<Self, QuarryError> {
        validate_config(config)?;
        let requested_workers = resolve_worker_count(threads)?;
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
        let bom_present = source_has_bom(&mut source, config.encoding)?;
        let mut output = ExportTarget::new_private_guarded(
            &source_path,
            destination.clone(),
            &source,
            source_stamp.clone(),
        )?;
        output.configure(config.encoding, config.encoding, config.max_record_bytes);
        let publication = output
            .defer_private_publication()
            .expect("duplicate output is private");
        let shared = Arc::new(DuplicateState {
            sort: Arc::new(SharedState::new(session.file_size)),
            duplicate_rows: Arc::new(AtomicU64::new(0)),
            retained_rows: AtomicU64::new(0),
        });
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-duplicates".into())
            .spawn(move || {
                let mut completion = WorkerCompletion::new(&worker_state.sort);
                let mut result = run_duplicates(
                    source,
                    &source_path,
                    &source_stamp,
                    delimiter,
                    has_header,
                    header_renames,
                    cell_edits,
                    spec,
                    destination,
                    output,
                    bom_present,
                    config,
                    &worker_state,
                    temporary_directory.as_deref(),
                    completed_record_count,
                    source_index,
                    requested_workers,
                    min_parallel_stripe_bytes,
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
            publication,
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
            effective_workers: shared.effective_workers.load(Ordering::Acquire),
            elapsed: shared.elapsed(),
            cancellation_latency: shared.cancellation_latency(),
            done: shared.done.load(Ordering::Acquire) || self.publication.is_ready(),
            cancelled: shared.cancelled.load(Ordering::Acquire),
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.sort.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.shared.sort.request_cancel();
        self.publication.decide(false);
    }

    /// Publish the candidate on the worker and transfer its lifetime to the
    /// caller. Before this call, drop discards only job-owned staging.
    pub fn wait(mut self) -> Result<DuplicateOutcome, QuarryError> {
        self.publication.decide(true);
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
            self.cancel();
            let _ = handle.join();
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
        self.start_find_duplicates_with_options(
            header_renames,
            cell_edits,
            spec,
            destination,
            DuplicateOptions::default(),
        )
    }

    pub fn start_find_duplicates_with_temp_directory(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        spec: DuplicateSpec,
        destination: impl AsRef<Path>,
        temporary_directory: impl AsRef<Path>,
    ) -> Result<DuplicateJob, QuarryError> {
        self.start_find_duplicates_with_options(
            header_renames,
            cell_edits,
            spec,
            destination,
            DuplicateOptions {
                temporary_directory: Some(temporary_directory.as_ref()),
                ..DuplicateOptions::default()
            },
        )
    }

    /// Analyze duplicates with explicit parallel striping, scratch directory,
    /// and adaptive single-run memory budget options.
    pub fn start_find_duplicates_with_options(
        &self,
        header_renames: BTreeMap<usize, Vec<u8>>,
        cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
        spec: DuplicateSpec,
        destination: impl AsRef<Path>,
        options: DuplicateOptions<'_>,
    ) -> Result<DuplicateJob, QuarryError> {
        let run_memory_bytes = match options.memory_budget_bytes {
            Some(0) => {
                return Err(QuarryError::InvalidOption(
                    "sort chunk, run memory, and record limit must be non-zero",
                ));
            }
            Some(bytes) => bytes,
            None => default_single_run_memory_budget_bytes(),
        };
        // Only trust a caller-supplied index when its provenance matches this session's exact
        // source file stamp, delimiter, encoding, byte length, and completed record count.
        let completed_records = self.completed_record_count();
        let validated_index = options
            .source_index
            .filter(|index| {
                index.matches_provenance(&self.source_stamp, self.dialect.delimiter, self.encoding)
                    && index.indexed_bytes() == self.file_size
                    && completed_records == Some(index.indexed_rows())
            })
            .cloned();
        DuplicateJob::start_with_options_internal(
            self,
            header_renames,
            cell_edits,
            spec,
            destination.as_ref().to_path_buf(),
            SortConfig {
                encoding: self.encoding,
                run_memory_bytes,
                ..DEFAULT_SORT_CONFIG
            },
            options.temporary_directory.map(Path::to_path_buf),
            completed_records,
            validated_index,
            options.threads,
            parallel::DEFAULT_MIN_PARALLEL_STRIPE_BYTES,
        )
    }
}

struct BuiltDuplicateOutput {
    rows_scanned: u64,
    retained_rows: u64,
    bytes_written: u64,
    index: StructuralIndex,
    in_memory_fast_path: bool,
}

#[allow(clippy::too_many_arguments)]
fn run_duplicates(
    mut source: File,
    source_path: &Path,
    source_stamp: &SourceStamp,
    delimiter: u8,
    has_header: bool,
    header_renames: BTreeMap<usize, Vec<u8>>,
    cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
    spec: DuplicateSpec,
    destination: PathBuf,
    mut output: ExportTarget,
    bom_present: bool,
    config: SortConfig,
    state: &DuplicateState,
    temporary_directory: Option<&Path>,
    completed_record_count: Option<u64>,
    source_index: Option<StructuralIndex>,
    requested_workers: usize,
    min_parallel_stripe_bytes: u64,
) -> Result<DuplicateOutcome, QuarryError> {
    let shared = &state.sort;
    let records = match completed_record_count {
        Some(records) => records,
        None => {
            shared.preparing.store(true, Ordering::Release);
            let Some(records) = crate::storage::count_records_encoded(
                &mut source,
                delimiter,
                config.encoding,
                &shared.cancel_requested,
                |bytes| {
                    shared
                        .preparation_bytes_scanned
                        .store(bytes, Ordering::Release);
                },
            )?
            else {
                return Ok(DuplicateOutcome::Cancelled);
            };
            records
        }
    };
    let output_bytes = estimate_edited_output_bytes(
        shared.total_bytes,
        records,
        &header_renames,
        &cell_edits,
        None,
        None,
    );
    let output_bytes = if config.encoding == Encoding::Utf8 {
        output_bytes
    } else {
        output_bytes.saturating_mul(3)
    };
    let temporary_directory = temporary_directory.unwrap_or_else(|| output_parent(&destination));
    check_operation_storage(
        temporary_directory,
        &destination,
        crate::estimate_duplicate_temporary_bytes(
            output_bytes,
            records.saturating_sub(u64::from(has_header)),
            spec.columns.len(),
        ),
        output_bytes,
    )?;
    let workspace = Arc::new(parallel::LazyRunWorkspace::new(
        temporary_directory.to_path_buf(),
    ));
    let source = Arc::new(source);
    let built = run_duplicates_inner(
        source,
        source_path,
        source_stamp,
        delimiter,
        has_header,
        Arc::new(header_renames),
        Arc::new(cell_edits),
        Arc::new(spec),
        &mut output,
        bom_present,
        config,
        &workspace,
        state,
        source_index.as_ref(),
        requested_workers,
        min_parallel_stripe_bytes,
    );
    let cleanup = Arc::try_unwrap(workspace)
        .ok()
        .expect("all duplicate workers drained before workspace cleanup")
        .cleanup();
    let built = built.map_err(|error| match error {
        QuarryError::Io(error) => storage_error(temporary_directory, error),
        error => error,
    })?;
    cleanup?;
    let Some(BuiltDuplicateOutput {
        rows_scanned,
        retained_rows,
        bytes_written,
        index,
        in_memory_fast_path,
    }) = built
    else {
        return Ok(DuplicateOutcome::Cancelled);
    };
    let (outcome, output_stamp) =
        output.publish_with_stamp(retained_rows, bytes_written, &shared.cancel_requested)?;
    match outcome {
        FilterExportOutcome::Cancelled => Ok(DuplicateOutcome::Cancelled),
        FilterExportOutcome::Complete(summary) => {
            Ok(DuplicateOutcome::Complete(DuplicateSummary {
                destination: summary.destination,
                output_index: output_stamp.and_then(|stamp| {
                    CompletedIndex::from_output(index, stamp, delimiter, config.encoding)
                }),
                rows_scanned,
                duplicate_rows: rows_scanned - retained_rows,
                retained_rows,
                bytes_written: summary.bytes_written,
                runs_created: shared.runs_created.load(Ordering::Acquire),
                peak_temporary_bytes: shared.peak_temporary_bytes.load(Ordering::Acquire),
                merge_passes: shared.merge_passes.load(Ordering::Acquire),
                header_rows: shared.header_rows.load(Ordering::Acquire),
                effective_workers: shared.effective_workers.load(Ordering::Acquire),
                in_memory_fast_path,
                elapsed: Duration::ZERO,
            }))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_duplicates_inner(
    source: Arc<File>,
    source_path: &Path,
    source_stamp: &SourceStamp,
    delimiter: u8,
    has_header: bool,
    header_renames: Arc<BTreeMap<usize, Vec<u8>>>,
    cell_edits: Arc<BTreeMap<(u64, usize), Vec<u8>>>,
    spec: Arc<DuplicateSpec>,
    output: &mut ExportTarget,
    bom_present: bool,
    config: SortConfig,
    workspace: &Arc<parallel::LazyRunWorkspace>,
    state: &DuplicateState,
    source_index: Option<&StructuralIndex>,
    requested_workers: usize,
    min_parallel_stripe_bytes: u64,
) -> Result<Option<BuiltDuplicateOutput>, QuarryError> {
    let shared = &state.sort;
    let pool = engine_pool();
    let Some(job_budget) = parallel::JobMemoryBudget::acquire_for_job(
        config.run_memory_bytes,
        pool.out_of_core_budget(),
        &shared.cancel_requested,
    ) else {
        return Ok(None);
    };
    let parallel::AdaptiveScanOutcome::Complete(mut scan) = parallel::create_initial_runs_adaptive(
        Arc::clone(&source),
        shared.total_bytes,
        delimiter,
        has_header,
        header_renames,
        cell_edits,
        parallel::AdaptiveRunOrder::Duplicates(spec),
        bom_present,
        config,
        Arc::clone(workspace),
        Arc::clone(shared),
        source_index,
        requested_workers,
        min_parallel_stripe_bytes,
        Arc::clone(&job_budget),
        Arc::clone(pool.large_record_gate()),
        Some(Arc::clone(&state.duplicate_rows)),
    )?
    else {
        return Ok(None);
    };
    shared
        .effective_workers
        .fetch_max(scan.effective_workers, Ordering::AcqRel);
    if !source_matches_stamp(&source, source_path, source_stamp)? {
        return Err(QuarryError::SourceChanged);
    }
    if shared.cancel_requested.load(Ordering::Acquire) {
        return Ok(None);
    }

    let in_memory_fast_path = scan.runs.is_empty();
    let mut index = StructuralIndex::new(IndexConfig::default())?;
    let header_width = match scan.header.as_deref() {
        Some(hdr) => parallel::count_record_fields(hdr, delimiter)?,
        None => 0,
    };

    let (retained, bytes_written, retained_max_width) = if in_memory_fast_path {
        let prefix_bytes = parallel::write_output_header_and_bom(
            output,
            bom_present,
            scan.header.as_deref(),
            &mut index,
            shared,
        )?;
        let Some((retained, data_bytes, data_max_width, _)) =
            parallel::deduplicate_in_memory_runs_to_output(
                &scan.in_memory_runs,
                delimiter,
                &state.duplicate_rows,
                &state.retained_rows,
                output,
                &scan.preferred_ending,
                config.max_record_bytes,
                prefix_bytes,
                if has_header { 0 } else { prefix_bytes },
                shared,
                Some(&mut index),
            )?
        else {
            return Ok(None);
        };
        drop(scan.in_memory_runs);
        job_budget.release_shared_reservation();
        if state.retained_rows.load(Ordering::Acquire)
            + state.duplicate_rows.load(Ordering::Acquire)
            != scan.rows
        {
            return Err(invalid_sort_output("duplicate run row count changed"));
        }
        if retained != state.retained_rows.load(Ordering::Acquire) {
            return Err(invalid_sort_output("retained duplicate row count changed"));
        }
        (
            retained,
            prefix_bytes.saturating_add(data_bytes),
            header_width.max(data_max_width),
        )
    } else {
        job_budget.release_shared_reservation();
        if !parallel::reduce_runs_parallel_with_dedup(
            &mut scan.runs,
            SortDirection::Ascending,
            scan.max_key_bytes,
            config,
            workspace,
            shared,
            scan.effective_workers,
            pool.fd_budget(),
            Some(Arc::clone(&state.duplicate_rows)),
        )? {
            return Ok(None);
        }
        let Some(fd_guard) = pool
            .fd_budget()
            .acquire(scan.runs.len().saturating_add(1), &shared.cancel_requested)
        else {
            return Ok(None);
        };
        let retained_result = retain_first_runs(&scan.runs, delimiter, config, workspace, state);
        drop(fd_guard);
        let Some((mut survivors, records, data_max_width)) = retained_result? else {
            return Ok(None);
        };
        if state.retained_rows.load(Ordering::Acquire)
            + state.duplicate_rows.load(Ordering::Acquire)
            != scan.rows
        {
            return Err(invalid_sort_output("duplicate run row count changed"));
        }
        remove_runs(&scan.runs, shared)?;
        if !parallel::reduce_runs_parallel(
            &mut survivors,
            SortDirection::Ascending,
            8,
            config,
            workspace,
            shared,
            scan.effective_workers,
            pool.fd_budget(),
        )? {
            return Ok(None);
        }
        let prefix_bytes = parallel::write_output_header_and_bom(
            output,
            bom_present,
            scan.header.as_deref(),
            &mut index,
            shared,
        )?;
        let Some((retained, data_bytes, _)) = merge_runs_to_output(
            &survivors,
            output,
            SortDirection::Ascending,
            &scan.preferred_ending,
            records,
            config.max_record_bytes,
            prefix_bytes,
            if has_header { 0 } else { prefix_bytes },
            shared,
            Some(&mut index),
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
        (
            retained,
            prefix_bytes.saturating_add(data_bytes),
            header_width.max(data_max_width),
        )
    };

    index.finish_output(bytes_written, retained_max_width);
    shared.bytes_written.store(bytes_written, Ordering::Release);
    Ok(Some(BuiltDuplicateOutput {
        rows_scanned: scan.rows,
        retained_rows: retained,
        bytes_written,
        index,
        in_memory_fast_path,
    }))
}

fn remove_runs(paths: &[SpillFile], shared: &SharedState) -> Result<(), QuarryError> {
    for path in paths {
        let bytes = path.metadata()?.len();
        path.remove()?;
        shared.remove_temporary_bytes(bytes);
    }
    Ok(())
}

fn retain_first_runs(
    paths: &[SpillFile],
    delimiter: u8,
    config: SortConfig,
    workspace: &parallel::LazyRunWorkspace,
    state: &DuplicateState,
) -> Result<Option<(Vec<SpillFile>, RecordMultisetFingerprint, usize)>, QuarryError> {
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
    const SURVIVOR_RUN_CAP_BYTES: usize = 32 * 1024 * 1024;
    let survivor_cap = config.run_memory_bytes.clamp(1, SURVIVOR_RUN_CAP_BYTES);
    let mut previous_key: Option<Vec<u8>> = None;
    let mut arena = parallel::RunArena::new();
    let mut runs = Vec::new();
    let mut records = RecordMultisetFingerprint::default();
    let mut retained_max_width = 0_usize;
    while let Some(item) = heap.pop() {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        let reader = &mut readers[item.run_index];
        if previous_key.as_ref() == Some(&item.head.key) {
            reader.seek_relative(item.head.record_len as i64)?;
            state.duplicate_rows.fetch_add(1, Ordering::AcqRel);
        } else {
            let record = read_record(reader, item.head.record_len, config.max_record_bytes)?;
            retained_max_width =
                retained_max_width.max(parallel::count_record_fields(&record, delimiter)?);
            let footprint = parallel::RunArena::entry_footprint(8, record.len());
            if !arena.is_empty() && arena.tracked_bytes().saturating_add(footprint) > survivor_cap {
                let Some((run, _)) = arena.flush_to_workspace_mode(
                    SortDirection::Ascending,
                    false,
                    workspace,
                    shared,
                    survivor_cap,
                )?
                else {
                    return Ok(None);
                };
                runs.push(run);
            }
            records.observe(&record);
            let ordinal_key = item.head.ordinal.to_be_bytes();
            arena.push_bounded(&ordinal_key, &record, item.head.ordinal, Some(survivor_cap));
            if footprint > survivor_cap {
                let Some((run, _)) = arena.flush_to_workspace_mode(
                    SortDirection::Ascending,
                    false,
                    workspace,
                    shared,
                    0,
                )?
                else {
                    return Ok(None);
                };
                runs.push(run);
            }
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
    if !arena.is_empty() {
        let Some((run, _)) =
            arena.flush_to_workspace_mode(SortDirection::Ascending, false, workspace, shared, 0)?
        else {
            return Ok(None);
        };
        runs.push(run);
    }
    if !paths.is_empty() {
        shared.merge_passes.fetch_add(1, Ordering::AcqRel);
    }
    Ok(Some((runs, records, retained_max_width)))
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
            encoding: Encoding::Utf8,
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
            assert!(summary.elapsed >= progress.elapsed);
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
        assert!(!small.destination.exists());
        fs::write(&small.destination, b"unrelated replacement").unwrap();
        drop(job);
        assert_eq!(
            fs::read(&small.destination).unwrap(),
            b"unrelated replacement"
        );
        assert!(small.artifacts().is_empty());
    }

    #[test]
    fn ready_duplicate_handoff_rejects_conflicts_and_cancellation() {
        for cancel in [false, true] {
            let fixture = Case::new(b"key\na\na\n");
            let job = fixture
                .session(HeaderMode::FirstRow)
                .start_find_duplicates(
                    BTreeMap::new(),
                    BTreeMap::new(),
                    spec(&[0], CaseSensitivity::Sensitive),
                    &fixture.destination,
                )
                .unwrap();
            wait_done(&job);
            assert!(!fixture.destination.exists());
            if cancel {
                job.cancel();
                assert_eq!(job.wait().unwrap(), DuplicateOutcome::Cancelled);
                assert!(!fixture.destination.exists());
            } else {
                fs::write(&fixture.destination, b"unrelated replacement").unwrap();
                assert!(matches!(
                    job.wait(),
                    Err(QuarryError::ExportDestinationExists)
                ));
                assert_eq!(
                    fs::read(&fixture.destination).unwrap(),
                    b"unrelated replacement"
                );
            }
            assert_eq!(fs::read(&fixture.source).unwrap(), b"key\na\na\n");
            assert!(fixture.artifacts().is_empty());
        }
    }

    #[test]
    fn source_replacement_and_destination_conflicts_never_publish() {
        let bytes = b"key,note\na,first\na,later\n";
        let fixture = Case::new(bytes);
        let session = fixture.session(HeaderMode::FirstRow);
        let source = File::open(&fixture.source).unwrap();
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
            sort: Arc::new(SharedState::new(bytes.len() as u64)),
            duplicate_rows: Arc::new(AtomicU64::new(0)),
            retained_rows: AtomicU64::new(0),
        };
        let result = run_duplicates(
            source,
            &fixture.source,
            &session.source_stamp,
            b',',
            true,
            BTreeMap::new(),
            BTreeMap::new(),
            spec(&[0], CaseSensitivity::Sensitive),
            fixture.destination.clone(),
            output,
            false,
            tiny_config(),
            &state,
            None,
            None,
            None,
            1,
            parallel::DEFAULT_MIN_PARALLEL_STRIPE_BYTES,
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
    #[test]
    fn first_native_record_limit_includes_headerless_duplicate_bom() {
        for encoding in [Encoding::Utf16Le, Encoding::Utf16Be] {
            for fits in [false, true] {
                let mut bytes = encoding.bom().to_vec();
                bytes.extend_from_slice(&encoding.encode(b"x\n", 4096).unwrap());
                let fixture = Case::new(&bytes);
                let session = Session::open_with_encoding(
                    &fixture.source,
                    QuarryOpenOptions {
                        header_mode: HeaderMode::NoHeader,
                        ..Default::default()
                    },
                    Some(encoding),
                )
                .unwrap();
                // Keep the duplicate key below the same bound while making the
                // native UTF-16 record exactly at or above it once the BOM is added.
                let value = vec![b'x'; if fits { 14 } else { 15 }];
                let job = DuplicateJob::start(
                    &session,
                    BTreeMap::new(),
                    BTreeMap::from([((0, 0), value)]),
                    spec(&[0], CaseSensitivity::Sensitive),
                    fixture.destination.clone(),
                    SortConfig {
                        encoding,
                        max_record_bytes: 32,
                        ..tiny_config()
                    },
                )
                .unwrap();
                let result = job.wait();
                if fits {
                    assert!(
                        matches!(result, Ok(DuplicateOutcome::Complete(_))),
                        "{encoding}: {result:?}"
                    );
                } else {
                    assert!(
                        matches!(result, Err(QuarryError::RecordTooLarge { limit: 32 })),
                        "{encoding}: {result:?}"
                    );
                    assert!(!fixture.destination.exists());
                }
                assert_eq!(fs::read(&fixture.source).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn parallel_duplicates_match_serial_in_memory_and_spilled_for_w_1_4_8() {
        let mut raw = UTF8_BOM.to_vec();
        raw.extend_from_slice(b"key,sub,seq,note\r\n");
        for i in 0..4_000_u64 {
            let key = if i % 2 == 0 {
                format!("K{:02}", i % 37)
            } else {
                format!("k{:02}", i % 37)
            };
            let sub = format!("S{}", i % 11);
            let note = if i % 29 == 0 {
                format!("\"multi\r\nline-{i}\"")
            } else {
                format!("row-{i}")
            };
            let ending = if i == 3_999 { "" } else { "\r\n" };
            raw.extend_from_slice(format!("{key},{sub},{i},{note}{ending}").as_bytes());
        }
        let fixture = Case::new(&raw);
        let session = fixture.session(HeaderMode::FirstRow);
        let index = session
            .start_indexing(IndexConfig {
                checkpoint_every: 128,
                ..IndexConfig::default()
            })
            .unwrap()
            .wait()
            .unwrap();
        let header_renames = BTreeMap::from([(0, b"KEY".to_vec())]);
        let cell_edits = BTreeMap::from([
            ((1, 0), b"EDITED_UNIQUE".to_vec()),
            ((2000, 3), b"edited,note".to_vec()),
        ]);

        for case in [CaseSensitivity::Insensitive, CaseSensitivity::Sensitive] {
            let dup_spec = spec(&[0, 1], case);
            let mut baseline: Option<Vec<u8>> = None;
            for (mode, memory_budget, merge_fan_in, expect_fast_path) in [
                ("in-memory", 32 * 1024 * 1024, 32, true),
                ("spilled", 8 * 1024, 3, false),
            ] {
                for workers in [1_usize, 4, 8] {
                    let dest = fixture
                        .directory
                        .join(format!("dup-{case:?}-{mode}-w{workers}.csv"));
                    let job = DuplicateJob::start_with_options_internal(
                        &session,
                        header_renames.clone(),
                        cell_edits.clone(),
                        dup_spec.clone(),
                        dest.clone(),
                        SortConfig {
                            run_memory_bytes: memory_budget,
                            merge_fan_in,
                            ..DEFAULT_SORT_CONFIG
                        },
                        Some(fixture.directory.clone()),
                        Some(index.indexed_rows()),
                        Some(index.clone()),
                        Some(workers),
                        1,
                    )
                    .unwrap();
                    let DuplicateOutcome::Complete(summary) = job.wait().unwrap() else {
                        panic!("{mode} w={workers} cancelled");
                    };
                    assert_eq!(summary.in_memory_fast_path, expect_fast_path);
                    if expect_fast_path {
                        assert_eq!(summary.runs_created, 0);
                        assert_eq!(summary.merge_passes, 0);
                    } else {
                        assert!(summary.runs_created > 0);
                    }
                    if workers > 1 {
                        assert!(summary.effective_workers > 1);
                    } else {
                        assert_eq!(summary.effective_workers, 1);
                    }
                    assert_eq!(summary.rows_scanned, 4_000);
                    assert_eq!(
                        summary.retained_rows + summary.duplicate_rows,
                        summary.rows_scanned
                    );
                    if let Some(output_index) = summary.output_index {
                        let out_session = Session::open(
                            &dest,
                            QuarryOpenOptions {
                                delimiter: Some(b','),
                                header_mode: HeaderMode::FirstRow,
                                ..QuarryOpenOptions::default()
                            },
                        )
                        .unwrap();
                        let adopted = out_session
                            .adopt_completed_index(output_index)
                            .expect("adopted duplicate output index");
                        let fresh = out_session
                            .start_indexing(IndexConfig::default())
                            .unwrap()
                            .wait()
                            .unwrap();
                        assert_eq!(adopted, fresh, "{mode} w={workers} output_index mismatch");
                    } else {
                        assert!(
                            !cfg!(target_os = "macos"),
                            "local macOS publication must retain duplicate index provenance"
                        );
                    }
                    assert!(fixture.artifacts().is_empty());

                    let bytes = fs::read(&dest).unwrap();
                    if let Some(ref expected) = baseline {
                        assert_eq!(&bytes, expected, "{case:?} {mode} w={workers} mismatch");
                    } else {
                        baseline = Some(bytes);
                    }
                }
            }
        }
    }

    #[test]
    fn parallel_duplicates_rejects_foreign_index_and_cancels_cleanly() {
        let mut a = String::from("k,v\n");
        let mut b = String::from("k,v\n");
        for i in 0..256 {
            a.push_str(&format!("{:02},{}\n", i % 19, "a".repeat(596)));
            let first = if i == 0 {
                100
            } else if i == 255 {
                300
            } else {
                200
            };
            b.push_str(&format!(
                "{:02},\"{}\n{}\"\n",
                i % 19,
                "b".repeat(first),
                "c".repeat(393)
            ));
        }
        assert_eq!(a.len(), b.len());
        let fa = Case::new(a.as_bytes());
        let fb = Case::new(b.as_bytes());
        let sa = fa.session(HeaderMode::FirstRow);
        let sb = fb.session(HeaderMode::FirstRow);
        let idx_a = sa
            .start_indexing(IndexConfig {
                checkpoint_every: 16,
                ..IndexConfig::default()
            })
            .unwrap()
            .wait()
            .unwrap();
        let idx_b = sb
            .start_indexing(IndexConfig {
                checkpoint_every: 16,
                ..IndexConfig::default()
            })
            .unwrap()
            .wait()
            .unwrap();

        let dest_local = fb.directory.join("local.csv");
        let DuplicateOutcome::Complete(local) = sb
            .start_find_duplicates_with_options(
                BTreeMap::new(),
                BTreeMap::new(),
                spec(&[0], CaseSensitivity::Sensitive),
                &dest_local,
                DuplicateOptions {
                    temporary_directory: Some(&fb.directory),
                    source_index: Some(&idx_b),
                    threads: Some(4),
                    memory_budget_bytes: Some(1024 * 1024),
                },
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("local cancelled");
        };
        assert!(local.effective_workers >= 2);
        assert_eq!(local.retained_rows, 19);
        assert_eq!(local.duplicate_rows, 256 - 19);

        let dest_foreign = fb.directory.join("foreign.csv");
        let DuplicateOutcome::Complete(foreign) = sb
            .start_find_duplicates_with_options(
                BTreeMap::new(),
                BTreeMap::new(),
                spec(&[0], CaseSensitivity::Sensitive),
                &dest_foreign,
                DuplicateOptions {
                    temporary_directory: Some(&fb.directory),
                    source_index: Some(&idx_a),
                    threads: Some(4),
                    memory_budget_bytes: Some(1024 * 1024),
                },
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("foreign cancelled");
        };
        assert_eq!(foreign.effective_workers, 1);
        assert_eq!(
            fs::read(&dest_foreign).unwrap(),
            fs::read(&dest_local).unwrap()
        );

        let dest_cancel = fb.directory.join("cancel.csv");
        let job = sb
            .start_find_duplicates_with_options(
                BTreeMap::new(),
                BTreeMap::new(),
                spec(&[0], CaseSensitivity::Sensitive),
                &dest_cancel,
                DuplicateOptions {
                    temporary_directory: Some(&fb.directory),
                    source_index: Some(&idx_b),
                    threads: Some(4),
                    memory_budget_bytes: Some(4 * 1024),
                },
            )
            .unwrap();
        job.cancel();
        assert_eq!(job.wait().unwrap(), DuplicateOutcome::Cancelled);
        assert!(!dest_cancel.exists());
        assert!(fb.artifacts().is_empty());
    }

    #[test]
    fn removed_wider_duplicate_row_does_not_inflate_output_index_width() {
        // Removing the wider duplicate row `a,b,c,d` leaves a 2-column file `a,b\n`; the
        // emitted output index must report width 2 (not 4) across both in-memory and spilled paths.
        let raw = b"a,b\na,b,c,d\n";
        let fixture = Case::new(raw);
        let session = fixture.session(HeaderMode::NoHeader);
        for (label, budget) in [("in-memory", 64 * 1024), ("spilled", 64)] {
            let dest = fixture.directory.join(format!("narrow-{label}.csv"));
            let job = DuplicateJob::start(
                &session,
                BTreeMap::new(),
                BTreeMap::new(),
                spec(&[0], CaseSensitivity::Sensitive),
                dest.clone(),
                SortConfig {
                    run_memory_bytes: budget,
                    ..tiny_config()
                },
            )
            .unwrap();
            let DuplicateOutcome::Complete(summary) = job.wait().unwrap() else {
                panic!("{label} cancelled");
            };
            assert_eq!(summary.retained_rows, 1);
            assert_eq!(summary.duplicate_rows, 1);
            assert_eq!(fs::read(&dest).unwrap(), b"a,b\n");
            if let Some(output_index) = summary.output_index {
                let out_session = Session::open(
                    &dest,
                    QuarryOpenOptions {
                        delimiter: Some(b','),
                        header_mode: HeaderMode::NoHeader,
                        ..QuarryOpenOptions::default()
                    },
                )
                .unwrap();
                let adopted = out_session
                    .adopt_completed_index(output_index)
                    .expect("adopt duplicate index");
                let fresh = out_session
                    .start_indexing(IndexConfig::default())
                    .unwrap()
                    .wait()
                    .unwrap();
                assert_eq!(adopted.maximum_width(), 2, "{label} width inflated");
                assert_eq!(adopted, fresh, "{label} output index mismatch");
            } else {
                assert!(!cfg!(target_os = "macos"));
            }
        }
    }
}
