//! Two-pass, source-preserving combination of equally shaped delimited files.
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};

use quarry_delimited::{RecordScanner, parse_record_with_field_limit};

use crate::export::{ExportTarget, source_matches_stamp};
use crate::storage::output_parent;
use crate::{
    DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, DEFAULT_SAMPLE_BYTES, FilterExportOutcome,
    MAX_TRANSFORMATION_COLUMNS, QuarryError, SourceStamp, check_storage, detect_delimiter,
};

const BOM: &[u8] = b"\xef\xbb\xbf";

#[derive(Clone, Copy, Debug)]
pub struct CombineOptions {
    /// None detects each input independently and requires the detected delimiters to match.
    pub delimiter: Option<u8>,
    /// Explicit for all inputs: never infer whether a data row is a header.
    pub has_header: bool,
}

impl Default for CombineOptions {
    fn default() -> Self {
        Self {
            delimiter: None,
            has_header: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CombineInput {
    pub path: PathBuf,
    pub bytes: u64,
    pub data_rows: u64,
    stamp: SourceStamp,
}

#[derive(Clone, Debug)]
pub struct CombinePlan {
    pub inputs: Vec<CombineInput>,
    pub columns: usize,
    pub delimiter: u8,
    pub has_header: bool,
    pub data_rows: u64,
    /// Exact output length, including a terminating LF added to unterminated records.
    pub output_bytes: u64,
}

#[derive(Debug)]
pub struct CombineSummary {
    pub destination: PathBuf,
    pub data_rows: u64,
    pub bytes_written: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct CombineProgress {
    pub file_index: usize,
    pub files: usize,
    pub bytes_scanned: u64,
    pub total_bytes: u64,
    pub done: bool,
}

struct Progress {
    file_index: AtomicUsize,
    files: usize,
    bytes: AtomicU64,
    total: AtomicU64,
    done: AtomicBool,
    cancel: AtomicBool,
}

struct Completion<'a>(&'a Progress);
impl Drop for Completion<'_> {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
    }
}

/// A bounded worker. None from wait() means cancellation; dropping also cancels and joins.
pub struct CombineJob<T> {
    state: Arc<Progress>,
    handle: Option<JoinHandle<Result<Option<T>, QuarryError>>>,
}

impl<T: Send + 'static> CombineJob<T> {
    fn spawn(
        files: usize,
        total: u64,
        work: impl FnOnce(&Progress) -> Result<Option<T>, QuarryError> + Send + 'static,
    ) -> Result<Self, QuarryError> {
        let state = Arc::new(Progress {
            file_index: AtomicUsize::new(0),
            files,
            bytes: AtomicU64::new(0),
            total: AtomicU64::new(total),
            done: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        });
        let worker = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("quarry-combine".into())
            .spawn(move || {
                let _completion = Completion(&worker);
                work(&worker)
            })?;
        Ok(Self {
            state,
            handle: Some(handle),
        })
    }

    pub fn wait(mut self) -> Result<Option<T>, QuarryError> {
        self.handle
            .take()
            .expect("combine worker exists")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl<T> CombineJob<T> {
    pub fn cancel(&self) {
        self.state.cancel.store(true, Ordering::Release);
    }
    pub fn progress(&self) -> CombineProgress {
        CombineProgress {
            file_index: self.state.file_index.load(Ordering::Acquire),
            files: self.state.files,
            bytes_scanned: self.state.bytes.load(Ordering::Acquire),
            total_bytes: self.state.total.load(Ordering::Acquire),
            done: self.state.done.load(Ordering::Acquire),
        }
    }
}

impl<T> Drop for CombineJob<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

fn input_error(path: &Path, record: Option<u64>, reason: impl ToString) -> QuarryError {
    QuarryError::CombineFile {
        path: path.to_path_buf(),
        record,
        reason: reason.to_string(),
    }
}

fn check_source(input: &CombineInput) -> Result<File, QuarryError> {
    let file = File::open(&input.path).map_err(|error| input_error(&input.path, None, error))?;
    if !source_matches_stamp(&file, &input.path, &input.stamp)? {
        return Err(input_error(
            &input.path,
            None,
            "File changed after validation. Check the files again.",
        ));
    }
    Ok(file)
}

/// Read and validate every record before offering a combined output for publication.
pub fn check_combine_files(
    paths: Vec<PathBuf>,
    options: CombineOptions,
) -> Result<CombineJob<CombinePlan>, QuarryError> {
    if paths.len() < 2 {
        return Err(QuarryError::InvalidOption(
            "select at least two files to combine",
        ));
    }
    CombineJob::spawn(paths.len(), 0, move |state| {
        let mut unique = HashSet::new();
        let mut inputs = Vec::with_capacity(paths.len());
        let mut delimiter = None;
        let mut header: Option<Vec<Vec<u8>>> = None;
        let mut width = None;
        let mut total_rows = 0_u64;
        let mut output_bytes = 0_u64;
        // Inspect one input at a time: neither descriptors nor bootstrap buffers accumulate.
        for (index, path) in paths.into_iter().enumerate() {
            if state.cancel.load(Ordering::Acquire) {
                return Ok(None);
            }
            state.file_index.store(index, Ordering::Release);
            let canonical = path
                .canonicalize()
                .map_err(|error| input_error(&path, None, error))?;
            if !unique.insert(canonical.clone()) {
                return Err(input_error(
                    &path,
                    None,
                    "This file was selected more than once.",
                ));
            }
            let mut file =
                File::open(&canonical).map_err(|error| input_error(&path, None, error))?;
            if !file.metadata()?.is_file() {
                return Err(input_error(&path, None, "Select a regular file."));
            }
            let stamp = SourceStamp::from_file(&file)?;
            let observed_delimiter = if let Some(delimiter) = options.delimiter {
                RecordScanner::new(delimiter)?;
                delimiter
            } else {
                let mut sample = Vec::new();
                file.by_ref()
                    .take(DEFAULT_SAMPLE_BYTES as u64)
                    .read_to_end(&mut sample)?;
                detect_delimiter(&sample, sample.len() as u64 == stamp.file_size())
            };
            if let Some(expected) = delimiter {
                if expected != observed_delimiter {
                    return Err(input_error(
                        &path,
                        None,
                        "Delimiter differs from the first file. Select files with the same delimiter.",
                    ));
                }
            } else {
                delimiter = Some(observed_delimiter);
            }
            let mut input = CombineInput {
                path: canonical,
                bytes: stamp.file_size(),
                data_rows: 0,
                stamp,
            };
            file.seek(SeekFrom::Start(0))?;
            drop(file);
            state.total.fetch_add(input.bytes, Ordering::Release);
            let mut source = check_source(&input)?;
            let mut records = 0_u64;
            let complete = scan(
                &mut source,
                &input,
                delimiter.unwrap(),
                state,
                |record, row| {
                    let content = if row == 0 {
                        record.strip_prefix(BOM).unwrap_or(record)
                    } else {
                        record
                    };
                    let fields = parse_record_with_field_limit(
                        content,
                        delimiter.unwrap(),
                        MAX_TRANSFORMATION_COLUMNS,
                    )
                    .map_err(|error| input_error(&input.path, Some(row + 1), error))?;
                    let expected = *width.get_or_insert(fields.len());
                    if fields.len() != expected {
                        return Err(input_error(
                            &input.path,
                            Some(row + 1),
                            format!(
                                "Expected {expected} columns, found {}. No output was written.",
                                fields.len()
                            ),
                        ));
                    }
                    if row == 0 && options.has_header {
                        if let Some(expected) = &header {
                            if let Some(column) = expected
                                .iter()
                                .zip(&fields)
                                .position(|(a, b)| a.as_slice() != b.as_ref())
                            {
                                return Err(input_error(
                                    &input.path,
                                    Some(1),
                                    format!(
                                        "Header column {} differs from the first file. Names and order must match exactly.",
                                        column + 1
                                    ),
                                ));
                            }
                        } else {
                            header = Some(fields.iter().map(|value| value.to_vec()).collect());
                        }
                    }
                    records += 1;
                    if included_record(options.has_header, index, row) {
                        let raw = output_record(record, index, row);
                        output_bytes = output_bytes
                            .checked_add(raw.len() as u64 + u64::from(!raw.ends_with(b"\n")))
                            .ok_or(QuarryError::InvalidOption(
                                "combined output size exceeds the supported limit",
                            ))?;
                    }
                    Ok(())
                },
            )?;
            if !complete {
                return Ok(None);
            }
            if records == 0 {
                return Err(input_error(
                    &input.path,
                    None,
                    "The file is empty. Remove it from the selection.",
                ));
            }
            check_source(&input)?;
            input.data_rows = records - u64::from(options.has_header);
            total_rows =
                total_rows
                    .checked_add(input.data_rows)
                    .ok_or(QuarryError::InvalidOption(
                        "combined row count exceeds the supported limit",
                    ))?;
            inputs.push(input);
        }
        for input in &inputs {
            if state.cancel.load(Ordering::Acquire) {
                return Ok(None);
            }
            check_source(input)?;
        }
        Ok(Some(CombinePlan {
            inputs,
            columns: width.unwrap(),
            delimiter: delimiter.unwrap(),
            has_header: options.has_header,
            data_rows: total_rows,
            output_bytes,
        }))
    })
}

impl CombinePlan {
    /// Create a new file atomically. Existing destinations and source aliases are never replaced.
    pub fn start_write(
        self,
        destination: PathBuf,
    ) -> Result<CombineJob<CombineSummary>, QuarryError> {
        let total = self
            .inputs
            .iter()
            .fold(0_u64, |total, input| total.saturating_add(input.bytes));
        CombineJob::spawn(self.inputs.len(), total, move |state| {
            if state.cancel.load(Ordering::Acquire) {
                return Ok(None);
            }
            check_storage(output_parent(&destination), self.output_bytes)?;
            let guards = self
                .inputs
                .iter()
                .map(|input| (input.path.clone(), input.stamp.clone()))
                .collect();
            let mut output = ExportTarget::new_combined(guards, destination.clone())?;
            let mut bytes_written = 0_u64;
            for (index, input) in self.inputs.iter().enumerate() {
                if state.cancel.load(Ordering::Acquire) {
                    return Ok(None);
                }
                state.file_index.store(index, Ordering::Release);
                let mut source = check_source(input)?;
                if !scan(&mut source, input, self.delimiter, state, |record, row| {
                    if included_record(self.has_header, index, row) {
                        let raw = output_record(record, index, row);
                        output.write_all(raw)?;
                        bytes_written += raw.len() as u64;
                        if !raw.ends_with(b"\n") {
                            output.write_all(b"\n")?;
                            bytes_written += 1;
                        }
                    }
                    Ok(())
                })? {
                    return Ok(None);
                }
                check_source(input)?;
            }
            if bytes_written != self.output_bytes {
                return Err(QuarryError::SourceChanged);
            }
            match output.publish(self.data_rows, bytes_written, &state.cancel)? {
                FilterExportOutcome::Complete(_) => Ok(Some(CombineSummary {
                    destination,
                    data_rows: self.data_rows,
                    bytes_written,
                })),
                FilterExportOutcome::Cancelled => Ok(None),
            }
        })
    }
}

fn included_record(has_header: bool, file: usize, row: u64) -> bool {
    !has_header || file == 0 || row != 0
}
fn output_record(record: &[u8], file: usize, row: u64) -> &[u8] {
    if file != 0 && row == 0 {
        record.strip_prefix(BOM).unwrap_or(record)
    } else {
        record
    }
}

// Only the current record and a fixed read chunk are retained. The limit is checked
// before appending, including records spanning multiple chunks and the final record.
fn scan(
    file: &mut File,
    input: &CombineInput,
    delimiter: u8,
    state: &Progress,
    mut record_fn: impl FnMut(&[u8], u64) -> Result<(), QuarryError>,
) -> Result<bool, QuarryError> {
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut chunk = vec![0; DEFAULT_READ_CHUNK];
    let mut record = Vec::new();
    let mut offset = 0_u64;
    let mut row = 0_u64;
    loop {
        if state.cancel.load(Ordering::Acquire) {
            return Ok(false);
        }
        let read = file
            .read(&mut chunk)
            .map_err(|error| input_error(&input.path, Some(row + 1), error))?;
        if read == 0 {
            let mut last = false;
            scanner
                .finish(offset, |_| last = true)
                .map_err(|error| input_error(&input.path, Some(row + 1), error))?;
            if last {
                record_fn(&record, row)?;
            }
            return Ok(!state.cancel.load(Ordering::Acquire));
        }
        if offset.saturating_add(read as u64) > input.bytes {
            return Err(input_error(
                &input.path,
                None,
                "File changed during processing.",
            ));
        }
        let mut start = 0;
        let mut error = None;
        let scanned = scanner.scan_chunk(&chunk[..read], offset, |end| {
            if error.is_some() || state.cancel.load(Ordering::Acquire) {
                return;
            }
            let end = (end - offset) as usize;
            if record.len().saturating_add(end - start) > DEFAULT_MAX_RECORD_BYTES {
                error = Some(input_error(
                    &input.path,
                    Some(row + 1),
                    "Record exceeds the 64 MiB limit.",
                ));
                return;
            }
            record.extend_from_slice(&chunk[start..end]);
            if let Err(found) = record_fn(&record, row) {
                error = Some(found);
            }
            record.clear();
            row += 1;
            start = end;
        });
        if let Some(error) = error {
            return Err(error);
        }
        if state.cancel.load(Ordering::Acquire) {
            return Ok(false);
        }
        scanned.map_err(|error| input_error(&input.path, Some(row + 1), error))?;
        if record.len().saturating_add(read - start) > DEFAULT_MAX_RECORD_BYTES {
            return Err(input_error(
                &input.path,
                Some(row + 1),
                "Record exceeds the 64 MiB limit.",
            ));
        }
        record.extend_from_slice(&chunk[start..read]);
        offset += read as u64;
        state.bytes.fetch_add(read as u64, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn inputs(dir: &Path, contents: &[&[u8]]) -> Vec<PathBuf> {
        contents
            .iter()
            .enumerate()
            .map(|(i, bytes)| {
                let path = dir.join(format!("input-{i}.csv"));
                fs::write(&path, bytes).unwrap();
                path
            })
            .collect()
    }
    fn plan(paths: Vec<PathBuf>, has_header: bool) -> Result<CombinePlan, QuarryError> {
        Ok(check_combine_files(
            paths,
            CombineOptions {
                has_header,
                ..CombineOptions::default()
            },
        )?
        .wait()?
        .unwrap())
    }

    #[test]
    fn combines_varied_file_counts_in_selected_order_with_duplicates_and_quoting() {
        for count in [2, 5, 10] {
            let dir = tempfile::tempdir().unwrap();
            let records = (0..count)
                .map(|i| format!("ID,Name\r\n{},\"line one\nline two\"", i / 2).into_bytes())
                .collect::<Vec<_>>();
            let mut paths = inputs(
                dir.path(),
                &records.iter().map(Vec::as_slice).collect::<Vec<_>>(),
            );
            paths.reverse();
            let before = paths
                .iter()
                .map(fs::read)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let plan = plan(paths.clone(), true).unwrap();
            assert_eq!(plan.inputs.len(), count);
            assert_eq!(plan.data_rows, count as u64);
            assert_eq!(plan.columns, 2);
            let mut expected = b"ID,Name\r\n".to_vec();
            for i in (0..count).rev() {
                expected
                    .extend_from_slice(format!("{},\"line one\nline two\"\n", i / 2).as_bytes());
            }
            assert_eq!(plan.output_bytes, expected.len() as u64);
            let destination = dir.path().join("combined.csv");
            let summary = plan
                .start_write(destination.clone())
                .unwrap()
                .wait()
                .unwrap()
                .unwrap();
            assert_eq!(summary.data_rows, count as u64);
            assert_eq!(summary.bytes_written, expected.len() as u64);
            assert_eq!(fs::read(destination).unwrap(), expected);
            for (path, bytes) in paths.iter().zip(before) {
                assert_eq!(fs::read(path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn order_unicode_boms_header_only_and_empty_cells_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = inputs(
            dir.path(),
            &[
                "\u{feff}ID,名称\r\n1,\"a,b\"\r\n".as_bytes(),
                "\u{feff}ID,名称\n2,\n".as_bytes(),
                "ID,名称\n".as_bytes(),
            ],
        );
        paths.swap(0, 1);
        let destination = dir.path().join("out.csv");
        plan(paths, true)
            .unwrap()
            .start_write(destination.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(destination).unwrap(),
            "\u{feff}ID,名称\n2,\n1,\"a,b\"\r\n".as_bytes()
        );
    }

    #[test]
    fn headerless_files_keep_their_first_rows_and_strip_only_input_boms() {
        let dir = tempfile::tempdir().unwrap();
        let paths = inputs(dir.path(), &[b"1,one", b"\xef\xbb\xbf2,two\n"]);
        let plan = plan(paths, false).unwrap();
        assert_eq!(plan.data_rows, 2);
        let destination = dir.path().join("out.csv");
        plan.start_write(destination.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"1,one\n2,two\n");
    }

    #[test]
    fn auto_detects_unterminated_header_only_and_single_record_inputs() {
        for delimiter in [',', '\t', '|', ';'] {
            for (first, second, has_header, rows, expected) in [
                (
                    format!("ID{delimiter}Name"),
                    format!("ID{delimiter}Name\n2{delimiter}two\n"),
                    true,
                    1,
                    format!("ID{delimiter}Name\n2{delimiter}two\n"),
                ),
                (
                    format!("ID{delimiter}Name"),
                    format!("ID{delimiter}Name"),
                    true,
                    0,
                    format!("ID{delimiter}Name\n"),
                ),
                (
                    format!("1{delimiter}\"first\nvalue\""),
                    format!("2{delimiter}second"),
                    false,
                    2,
                    format!("1{delimiter}\"first\nvalue\"\n2{delimiter}second\n"),
                ),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let paths = inputs(dir.path(), &[first.as_bytes(), second.as_bytes()]);
                let opened =
                    crate::Session::open(&paths[0], crate::OpenOptions::default()).unwrap();
                assert_eq!(opened.dialect.delimiter, delimiter as u8);
                assert_eq!(opened.first_rows[0].fields.len(), 2);
                let checked = plan(paths, has_header).unwrap();
                assert_eq!(checked.delimiter, delimiter as u8);
                assert_eq!(checked.columns, 2);
                assert_eq!(checked.data_rows, rows);
                let output = dir.path().join("out.csv");
                checked
                    .start_write(output.clone())
                    .unwrap()
                    .wait()
                    .unwrap()
                    .unwrap();
                assert_eq!(fs::read(&output).unwrap(), expected.as_bytes());
                let reopened =
                    crate::Session::open(&output, crate::OpenOptions::default()).unwrap();
                assert_eq!(reopened.dialect.delimiter, delimiter as u8);
                assert_eq!(reopened.first_rows[0].fields.len(), 2);
            }
        }
    }

    #[test]
    fn rejects_header_order_case_spaces_and_late_width_mismatches() {
        for bad in [
            b"Name,ID\na,2\n".as_slice(),
            b"id,Name\n2,b\n",
            b"ID,Name \n2,b\n",
            b"ID,Name\n2,b\n3\n",
            b"ID,Name\n2,b,c\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let paths = inputs(dir.path(), &[b"ID,Name\n1,a\n", bad]);
            let error = plan(paths, true).unwrap_err().to_string();
            assert!(error.contains("input-1.csv"), "{error}");
            assert!(error.contains("record"), "{error}");
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        }
    }

    #[test]
    fn rejects_empty_duplicate_malformed_and_differently_delimited_files() {
        for bad in [
            b"".as_slice(),
            b"ID\tName\n2\tb\n",
            b"ID,Name\n2,\"unfinished",
        ] {
            let dir = tempfile::tempdir().unwrap();
            assert!(plan(inputs(dir.path(), &[b"ID,Name\n1,a\n", bad]), true).is_err());
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = inputs(dir.path(), &[b"a,b\n"]);
        assert!(check_combine_files(paths.clone(), CombineOptions::default()).is_err());
        assert!(
            plan(vec![paths[0].clone(), paths[0].clone()], true)
                .unwrap_err()
                .to_string()
                .contains("more than once")
        );
    }

    #[test]
    fn stale_inputs_and_existing_destinations_never_publish_or_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let paths = inputs(dir.path(), &[b"ID,Name\n1,a\n", b"ID,Name\n2,b\n"]);
        let checked = plan(paths.clone(), true).unwrap();
        let destination = dir.path().join("out.csv");
        fs::write(&destination, b"keep me").unwrap();
        assert!(
            checked
                .clone()
                .start_write(destination.clone())
                .unwrap()
                .wait()
                .is_err()
        );
        assert_eq!(fs::read(&destination).unwrap(), b"keep me");
        fs::remove_file(&destination).unwrap();
        fs::write(&paths[0], b"ID,Name\n9,a\n").unwrap();
        assert!(
            checked
                .start_write(destination.clone())
                .unwrap()
                .wait()
                .is_err()
        );
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        let checked = plan(paths.clone(), true).unwrap();
        assert!(
            checked
                .start_write(paths[1].clone())
                .unwrap()
                .wait()
                .is_err()
        );
        assert_eq!(fs::read(&paths[1]).unwrap(), b"ID,Name\n2,b\n");
    }

    #[test]
    fn cancelled_scan_stops_within_a_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let paths = inputs(dir.path(), &[b"a,b\n1,2\n", b"a,b\n3,4\n"]);
        let checked = plan(paths, true).unwrap();
        let input = &checked.inputs[0];
        let state = Progress {
            file_index: AtomicUsize::new(0),
            files: 2,
            bytes: AtomicU64::new(0),
            total: AtomicU64::new(input.bytes),
            done: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        };
        let mut file = check_source(input).unwrap();
        let mut visited = 0;
        assert!(
            !scan(&mut file, input, b',', &state, |_, _| {
                visited += 1;
                state.cancel.store(true, Ordering::Release);
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(visited, 1);
    }

    #[test]
    fn records_crossing_read_chunks_keep_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let large = [
            b"ID,Name\n1,\"".as_slice(),
            &vec![b'x'; DEFAULT_READ_CHUNK + 5],
            b"\nend\"",
        ]
        .concat();
        let paths = inputs(dir.path(), &[&large, b"ID,Name\n2,last\n"]);
        let destination = dir.path().join("out.csv");
        plan(paths, true)
            .unwrap()
            .start_write(destination.clone())
            .unwrap()
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(destination).unwrap(),
            [large.as_slice(), b"\n2,last\n"].concat()
        );
    }

    #[test]
    fn publication_rechecks_every_input_and_cleans_up_on_conflict_or_cancel() {
        for cancel in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = inputs(dir.path(), &[b"ID,Name\n1,a\n", b"ID,Name\n2,b\n"]);
            let checked = plan(paths.clone(), true).unwrap();
            let destination = dir.path().join("out.csv");
            let guards = checked
                .inputs
                .iter()
                .map(|input| (input.path.clone(), input.stamp.clone()))
                .collect();
            let mut target = ExportTarget::new_combined(guards, destination.clone()).unwrap();
            target.write_all(b"ID,Name\n1,a\n2,b\n").unwrap();
            if !cancel {
                fs::write(&paths[1], b"ID,Name\nchanged,b\n").unwrap();
            }
            let result = target.publish(2, 20, &AtomicBool::new(cancel));
            if cancel {
                assert!(matches!(result, Ok(FilterExportOutcome::Cancelled)));
            } else {
                assert!(result.unwrap_err().to_string().contains("input-1.csv"));
            }
            assert!(!destination.exists());
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        }
    }

    #[test]
    fn panicking_worker_still_becomes_pollable_and_reports_failure() {
        let job = CombineJob::<()>::spawn(2, 0, |_| panic!("test worker failure")).unwrap();
        while !job.progress().done {
            thread::yield_now();
        }
        assert!(matches!(job.wait(), Err(QuarryError::WorkerPanicked)));
    }
}
