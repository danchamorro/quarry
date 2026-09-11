//! Capacity checks are advisory; every writer still cleans up on a later error.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use quarry_delimited::RecordScanner;

use crate::{ColumnTransformation, LiteralReplacement, QuarryError};

static NEXT_PROBE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageSpace {
    pub directory: PathBuf,
    /// Additional bytes, excluding versions already on disk and reflected in availability.
    pub required_bytes: u64,
    pub available_bytes: u64,
}

impl StorageSpace {
    pub fn require(mut self, additional_bytes: u64) -> Result<Self, QuarryError> {
        self.required_bytes = additional_bytes;
        if additional_bytes > self.available_bytes {
            return Err(QuarryError::InsufficientStorage {
                directory: self.directory,
                required_bytes: additional_bytes,
                available_bytes: self.available_bytes,
            });
        }
        Ok(self)
    }
}

pub(crate) fn storage_error(directory: &Path, error: io::Error) -> QuarryError {
    QuarryError::Storage {
        directory: directory.to_path_buf(),
        error,
    }
}

pub(crate) fn output_parent(destination: &Path) -> &Path {
    destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Inspect the actual volume and verify creating, writing, and removing a private file.
/// Run this on a worker thread when a removable or network drive could be slow.
pub fn inspect_storage(directory: &Path) -> Result<StorageSpace, QuarryError> {
    let result = (|| {
        if !fs::metadata(directory)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "location must be a directory",
            ));
        }
        let available_bytes = available_space(directory)?;
        for _ in 0..100 {
            let id = NEXT_PROBE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(".quarry-probe-{}-{id}", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&path) {
                Ok(mut file) => {
                    let written = file.write_all(&[0]);
                    drop(file);
                    let removed = fs::remove_file(&path);
                    written?;
                    removed?;
                    return Ok(StorageSpace {
                        directory: directory.to_path_buf(),
                        required_bytes: 0,
                        available_bytes,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a storage probe",
        ))
    })();
    result.map_err(|error| storage_error(directory, error))
}

pub fn check_storage(
    directory: &Path,
    additional_required_bytes: u64,
) -> Result<StorageSpace, QuarryError> {
    inspect_storage(directory)?.require(additional_required_bytes)
}

#[cfg(unix)]
fn available_space(directory: &Path) -> io::Result<u64> {
    let space = rustix::fs::statvfs(directory)?;
    Ok(space.f_bavail.saturating_mul(space.f_frsize))
}

#[cfg(windows)]
fn available_space(directory: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let path: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
    if path[..path.len() - 1].contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "storage path contains NUL",
        ));
    }
    let mut available = 0;
    // SAFETY: path is NUL-terminated; available is a valid writable u64, and the
    // optional total-byte outputs are null as permitted by GetDiskFreeSpaceExW.
    if unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

#[cfg(not(any(unix, windows)))]
fn available_space(_: &Path) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "available-space checks are unsupported on this platform",
    ))
}

/// The spill estimate includes the final output when both locations share a volume.
pub(crate) fn check_operation_storage(
    temporary: &Path,
    destination: &Path,
    total_bytes: u64,
    output_bytes: u64,
) -> Result<(), QuarryError> {
    let destination = output_parent(destination);
    let temporary_space = inspect_storage(temporary)?;
    let output_space = inspect_storage(destination)?;
    #[cfg(unix)]
    let same_volume = fs::metadata(temporary)
        .map_err(|error| storage_error(temporary, error))?
        .dev()
        == fs::metadata(destination)
            .map_err(|error| storage_error(destination, error))?
            .dev();
    // Unknown volume identity must not undercount simultaneous allocation.
    #[cfg(not(unix))]
    let same_volume = true;
    temporary_space.require(total_bytes)?;
    output_space.require(if same_volume {
        total_bytes
    } else {
        output_bytes
    })?;
    Ok(())
}

/// Conservative serialized output bound. Record count includes the header.
/// Includes whole-row requoting of bare CR and BOM-looking first fields. Saturation fails
/// safely at the subsequent capacity check instead of wrapping an estimate.
pub fn estimate_edited_output_bytes(
    source_bytes: u64,
    records: u64,
    headers: &BTreeMap<usize, Vec<u8>>,
    cells: &BTreeMap<(u64, usize), Vec<u8>>,
    transformation: Option<&ColumnTransformation>,
    replacement: Option<&LiteralReplacement>,
) -> u64 {
    let serialized = |value: &[u8]| (value.len() as u64).saturating_mul(2).saturating_add(2);
    let overlay = headers
        .values()
        .chain(cells.values())
        .fold(0_u64, |sum, value| sum.saturating_add(serialized(value)));
    let mut previous_row = None;
    let edited_rows = cells
        .keys()
        .fold(0_u64, |count, (row, _)| {
            let new_row = previous_row != Some(*row);
            previous_row = Some(*row);
            count.saturating_add(u64::from(new_row))
        })
        .saturating_add(u64::from(!headers.is_empty()));
    // Sort/duplicate materialization may quote every BOM-looking first field,
    // including rows without edits. Sparse saves pass zero and use edited rows.
    let mut bytes = source_bytes
        .saturating_add(overlay)
        .saturating_add(records.max(edited_rows).saturating_mul(2));
    // Bare CR is legal unquoted input. Reserializing a row may quote every
    // such field, not just the edited or BOM-looking field.
    if records > 0 || edited_rows > 0 || transformation.is_some() || replacement.is_some() {
        bytes = bytes.saturating_mul(2);
    }
    if let Some(transformation) = transformation {
        match transformation {
            ColumnTransformation::Split {
                source_column,
                output_count,
                output_headers,
                ..
            } => {
                bytes =
                    bytes.saturating_add(records.saturating_mul(
                        (*source_column as u64).saturating_add(*output_count as u64),
                    ));
                if let Some(headers) = output_headers {
                    bytes = headers
                        .iter()
                        .fold(bytes, |sum, header| sum.saturating_add(serialized(header)));
                }
            }
            ColumnTransformation::Join {
                source_columns,
                separator,
                output_header,
            } => {
                let padding = source_columns.iter().max().copied().unwrap_or(0) as u64;
                let separators = (separator.len() as u64)
                    .saturating_mul(source_columns.len().saturating_sub(1) as u64)
                    .saturating_mul(2);
                bytes = bytes
                    .saturating_add(records.saturating_mul(padding.saturating_add(separators)));
                if let Some(header) = output_header {
                    bytes = bytes.saturating_add(serialized(header));
                }
            }
            ColumnTransformation::Arrange { output_columns, .. } => {
                bytes = bytes.saturating_add(records.saturating_mul(output_columns.len() as u64));
            }
        }
    }
    if let Some(replacement) = replacement {
        let needle = replacement.needle.len().max(1) as u64;
        let growth = (replacement.replacement.len() as u64).saturating_sub(needle);
        bytes = bytes.saturating_add((bytes / needle).saturating_mul(growth));
    }
    bytes.saturating_add(3) // A leading BOM-looking data field may need disambiguation.
}

/// Two generations of records, decoded keys and eight-byte per-field framing.
/// Also covers the survivor-order merge and final candidate output.
pub const fn estimate_duplicate_temporary_bytes(
    effective_bytes: u64,
    rows: u64,
    selected_columns: usize,
) -> u64 {
    effective_bytes.saturating_mul(4).saturating_add(
        rows.saturating_mul(64_u64.saturating_add((selected_columns as u64).saturating_mul(16))),
    )
}

/// Count logical records without retaining them, then rewind for the operation.
/// This avoids assuming a byte is a row and rejecting ordinary large files.
pub(crate) fn count_records(
    source: &mut File,
    delimiter: u8,
    cancel: &AtomicBool,
) -> Result<Option<u64>, QuarryError> {
    count_records_with_progress(source, delimiter, cancel, |_| {})
}

pub(crate) fn count_records_with_progress(
    source: &mut File,
    delimiter: u8,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<Option<u64>, QuarryError> {
    source.seek(SeekFrom::Start(0))?;
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut chunk = vec![0; crate::DEFAULT_READ_CHUNK];
    let mut offset = 0;
    let mut records = 0_u64;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(None);
        }
        let read = source.read(&mut chunk)?;
        if read == 0 {
            scanner.finish(offset, |_| records = records.saturating_add(1))?;
            source.seek(SeekFrom::Start(0))?;
            return Ok(Some(records));
        }
        scanner.scan_chunk(&chunk[..read], offset, |_| {
            records = records.saturating_add(1)
        })?;
        offset += read as u64;
        progress(offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_additional_and_reports_exact_shortfall() {
        let space = StorageSpace {
            directory: PathBuf::from("/chosen"),
            required_bytes: 0,
            available_bytes: 100,
        };
        assert_eq!(space.clone().require(100).unwrap().required_bytes, 100);
        assert!(matches!(
            space.require(101),
            Err(QuarryError::InsufficientStorage {
                required_bytes: 101,
                available_bytes: 100,
                ..
            })
        ));
        assert!(estimate_duplicate_temporary_bytes(u64::MAX, 1, 1) == u64::MAX);
    }

    #[test]
    fn output_bound_covers_bom_looking_first_fields_in_unedited_rows() {
        use crate::{
            CaseSensitivity, DuplicateOutcome, DuplicateSpec, HeaderMode, OpenOptions, Session,
            SortDirection, SortMode, SortOutcome, SortSpec,
        };
        let id = NEXT_PROBE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("quarry-storage-bom-{}-{id}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let bytes = b"key,a,b,c,d,e\n\xef\xbb\xbfc,a\r,a\r,a\r,a\r,x\n\xef\xbb\xbfa,a\r,a\r,a\r,a\r,x\n\xef\xbb\xbfb,a\r,a\r,a\r,a\r,x\n";
        fs::write(&source, bytes).unwrap();
        let session = Session::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                delimiter: Some(b','),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let required = estimate_edited_output_bytes(
            bytes.len() as u64,
            4,
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
        );
        let SortOutcome::Complete(sorted) = session
            .start_create_sorted_working_copy(
                BTreeMap::new(),
                BTreeMap::new(),
                SortSpec {
                    column: 0,
                    mode: SortMode::Text,
                    direction: SortDirection::Ascending,
                    case_sensitivity: CaseSensitivity::Sensitive,
                },
                directory.join("sorted.csv"),
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("unexpected cancellation")
        };
        let DuplicateOutcome::Complete(duplicates) = session
            .start_find_duplicates(
                BTreeMap::new(),
                BTreeMap::new(),
                DuplicateSpec {
                    columns: vec![0],
                    case_sensitivity: CaseSensitivity::Sensitive,
                },
                directory.join("duplicates.csv"),
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("unexpected cancellation")
        };
        assert!(sorted.bytes_written > bytes.len() as u64 + 2 * 4 + 3);
        let sparse_cells = BTreeMap::from([((1, 5), b"y".to_vec())]);
        let sparse_required = estimate_edited_output_bytes(
            bytes.len() as u64,
            0,
            &BTreeMap::new(),
            &sparse_cells,
            None,
            None,
        );
        let crate::SaveAsOutcome::Complete(saved) = session
            .start_save_as_with_edits(BTreeMap::new(), sparse_cells, directory.join("edited.csv"))
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("unexpected cancellation")
        };
        assert!(sparse_required >= saved.bytes_written);
        assert!(required >= sorted.bytes_written);
        assert!(required >= duplicates.bytes_written);
        assert_eq!(fs::read(&source).unwrap(), bytes);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn probe_checks_chosen_location_and_removes_its_file() {
        let id = NEXT_PROBE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("quarry-storage-test-{}-{id}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        assert!(inspect_storage(&directory).unwrap().available_bytes > 0);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        assert!(matches!(
            check_storage(&directory, u64::MAX),
            Err(QuarryError::InsufficientStorage { .. })
        ));
        fs::remove_dir(&directory).unwrap();
        assert!(matches!(
            inspect_storage(&directory),
            Err(QuarryError::Storage { .. })
        ));
    }

    #[test]
    fn estimates_cover_actual_expansion_and_counting_preserves_multiline_records() {
        use crate::{HeaderMode, OpenOptions, SaveAsOutcome, Session};
        let id = NEXT_PROBE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "quarry-storage-estimates-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.csv");
        let bytes = b"a,b,c\n\"one\nline\",x,y\nz,,\n";
        fs::write(&source, bytes).unwrap();
        let mut file = File::open(&source).unwrap();
        assert_eq!(
            count_records(&mut file, b',', &AtomicBool::new(false)).unwrap(),
            Some(3)
        );
        assert_eq!(file.stream_position().unwrap(), 0);
        assert_eq!(
            count_records(&mut file, b',', &AtomicBool::new(true)).unwrap(),
            None
        );
        let cancel = AtomicBool::new(false);
        let mut reported_bytes = 0;
        assert_eq!(
            count_records_with_progress(&mut file, b',', &cancel, |bytes| {
                reported_bytes = bytes;
                cancel.store(true, Ordering::Release);
            })
            .unwrap(),
            None
        );
        assert_eq!(reported_bytes, bytes.len() as u64);
        let session = Session::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                delimiter: Some(b','),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let headers = BTreeMap::from([(0, b"wide,header".to_vec())]);
        let cells = BTreeMap::from([((1, 0), b"expanded,\"value\"|two".to_vec())]);
        let transformations = [
            ColumnTransformation::Arrange {
                source_width: 3,
                output_columns: vec![2, 0],
            },
            ColumnTransformation::Split {
                source_column: 0,
                separator: b"|".to_vec(),
                output_count: 4,
                output_headers: Some(vec![
                    b"first".to_vec(),
                    b"second".to_vec(),
                    Vec::new(),
                    Vec::new(),
                ]),
            },
            ColumnTransformation::Join {
                source_columns: vec![2, 0, 1],
                separator: b"\",\"".to_vec(),
                output_header: Some(b"joined".to_vec()),
            },
        ];
        for (index, transformation) in transformations.into_iter().enumerate() {
            let required = estimate_edited_output_bytes(
                bytes.len() as u64,
                3,
                &headers,
                &cells,
                Some(&transformation),
                None,
            );
            let destination = directory.join(format!("output-{index}.csv"));
            let SaveAsOutcome::Complete(output) = session
                .start_save_as_with_transformation(
                    headers.clone(),
                    cells.clone(),
                    transformation,
                    &destination,
                )
                .unwrap()
                .wait()
                .unwrap()
            else {
                panic!("unexpected cancellation")
            };
            assert!(required >= output.bytes_written);
        }
        fs::remove_dir_all(directory).unwrap();
    }
}
