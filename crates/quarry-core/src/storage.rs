//! Capacity checks are advisory; every writer still cleans up on a later error.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

/// Strictly validate a user-selected scratch directory before use or persistence:
/// must be an existing absolute directory, must not traverse or target a symlink / reparse point,
/// must be owned by the current user, and must support private temporary storage creation.
pub fn validate_scratch_directory(directory: &Path) -> Result<StorageSpace, QuarryError> {
    let result = (|| -> io::Result<StorageSpace> {
        if directory.as_os_str().is_empty() || !directory.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "temporary folder must be an absolute path",
            ));
        }
        reject_scratch_symlink_components(directory)?;
        let metadata = fs::symlink_metadata(directory)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "temporary folder cannot be a symbolic link",
            ));
        }
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "location must be a directory",
            ));
        }
        #[cfg(unix)]
        {
            if metadata.uid() != rustix::process::geteuid().as_raw() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "temporary folder must be owned by the current user",
                ));
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
                FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            const READ_CONTROL: u32 = 0x0002_0000;
            if (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "temporary folder cannot be a reparse point or symlink",
                ));
            }
            let dir_handle = OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(directory)?;
            let dir_meta = dir_handle.metadata()?;
            if !dir_meta.is_dir()
                || (dir_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "temporary folder cannot be a reparse point or symlink",
                ));
            }
            if !crate::sidecar::verify_windows_handle_owned_by_current_user(
                dir_handle.as_raw_handle(),
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "temporary folder must be owned by the current user",
                ));
            }
        }

        // Verify the volume can create an owner-only private workspace (including POSIX 0700 + ACL checks)
        // and write/remove a private probe file inside it.
        let available_bytes = available_space(directory)?;
        let probe_workspace = crate::spill::SpillWorkspace::create_in(directory, "probe").map_err(
            |error| match error {
                QuarryError::Storage { error, .. } | QuarryError::Io(error) => error,
                other => io::Error::other(other.to_string()),
            },
        )?;
        let probe_result = (|| -> io::Result<()> {
            let (run, mut writer) = probe_workspace.create_run().map_err(|error| match error {
                QuarryError::Storage { error, .. } | QuarryError::Io(error) => error,
                other => io::Error::other(other.to_string()),
            })?;
            writer.write_all(&[0])?;
            writer.flush()?;
            drop(writer);
            run.remove()?;
            Ok(())
        })();
        let cleanup_result = probe_workspace.cleanup().map_err(|error| match error {
            QuarryError::Storage { error, .. } | QuarryError::Io(error) => error,
            other => io::Error::other(other.to_string()),
        });
        probe_result?;
        cleanup_result?;
        Ok(StorageSpace {
            directory: directory.to_path_buf(),
            required_bytes: 0,
            available_bytes,
        })
    })();
    result.map_err(|error| storage_error(directory, error))
}

fn reject_scratch_symlink_components(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        if matches!(
            component,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        let meta = fs::symlink_metadata(&current)?;
        // On macOS, `/var` and `/tmp` are OS-level root symlinks to `/private/*`.
        #[cfg(target_os = "macos")]
        if current == Path::new("/var") || current == Path::new("/tmp") {
            continue;
        }
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "temporary folder cannot traverse or target a symbolic link",
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "temporary folder cannot traverse or target a reparse point",
                ));
            }
        }
    }
    Ok(())
}

/// Query advisory available disk bytes on the volume containing `directory` without writing a
/// probe file. Run this off the UI thread so slow or network volumes never block rendering.
pub fn available_disk_space(directory: &Path) -> Result<u64, QuarryError> {
    available_space(directory).map_err(|error| storage_error(directory, error))
}

#[cfg(unix)]
pub(crate) fn available_space(directory: &Path) -> io::Result<u64> {
    let space = rustix::fs::statvfs(directory)?;
    Ok(space.f_bavail.saturating_mul(space.f_frsize))
}

#[cfg(windows)]
pub(crate) fn available_space(directory: &Path) -> io::Result<u64> {
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
pub(crate) fn available_space(_: &Path) -> io::Result<u64> {
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
    estimate_edited_output_bytes_from_values(
        source_bytes,
        records,
        headers
            .iter()
            .map(|(column, value)| (*column, value.as_slice())),
        cells.iter().map(|(key, value)| (*key, value.as_slice())),
        transformation,
        replacement,
    )
}

/// Estimate borrowed overlays without copying their field bytes. Cell iteration should
/// follow row order; repeated nonconsecutive rows conservatively overestimate the result.
pub fn estimate_edited_output_bytes_from_values<'a>(
    source_bytes: u64,
    records: u64,
    headers: impl IntoIterator<Item = (usize, &'a [u8])>,
    cells: impl IntoIterator<Item = ((u64, usize), &'a [u8])>,
    transformation: Option<&ColumnTransformation>,
    replacement: Option<&LiteralReplacement>,
) -> u64 {
    let serialized = |value: &[u8]| (value.len() as u64).saturating_mul(2).saturating_add(2);
    let mut overlay = 0_u64;
    let mut header_edited = false;
    for (_, value) in headers {
        header_edited = true;
        overlay = overlay.saturating_add(serialized(value));
    }
    let mut previous_row = None;
    let mut edited_rows = u64::from(header_edited);
    for ((row, _), value) in cells {
        overlay = overlay.saturating_add(serialized(value));
        edited_rows = edited_rows.saturating_add(u64::from(previous_row != Some(row)));
        previous_row = Some(row);
    }
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
            ColumnTransformation::Cleanup(cleanup) => {
                match &cleanup.operation {
                    crate::CleanupOperation::Trim | crate::CleanupOperation::CollapseSpaces => {}
                    crate::CleanupOperation::Uppercase | crate::CleanupOperation::Lowercase => {
                        // Unicode full case mappings contain at most three scalars per input scalar.
                        bytes = bytes.saturating_mul(3);
                    }
                    crate::CleanupOperation::FillEmpty(value) => {
                        bytes = bytes.saturating_add(
                            records
                                .saturating_mul(cleanup.columns.len() as u64)
                                .saturating_mul(serialized(value)),
                        );
                    }
                }
            }
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
            ColumnTransformation::Insert {
                source_width,
                count,
                headers,
                ..
            } => {
                bytes = bytes.saturating_add(
                    records.saturating_mul((*source_width as u64).saturating_add(*count as u64)),
                );
                if let Some(headers) = headers {
                    bytes = headers
                        .iter()
                        .fold(bytes, |sum, name| sum.saturating_add(serialized(name)));
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

/// Conservative disk allowance for a duplicate target. Whole-row (`AllFields`) frames store
/// each record once with a fixed 24-byte comparison key, avoiding payload doubling across
/// merge generations.
pub fn estimate_duplicate_target_temporary_bytes(
    effective_bytes: u64,
    rows: u64,
    target: &crate::DuplicateTarget,
) -> u64 {
    match target {
        crate::DuplicateTarget::Columns(columns) => {
            estimate_duplicate_temporary_bytes(effective_bytes, rows, columns.len())
        }
        crate::DuplicateTarget::AllFields => effective_bytes
            .saturating_mul(2)
            .saturating_add(rows.saturating_mul(112)),
    }
}

/// Count logical records without retaining them, then rewind for the operation.
/// This avoids assuming a byte is a row and rejecting ordinary large files.
#[cfg(test)]
pub(crate) fn count_records(
    source: &mut File,
    delimiter: u8,
    cancel: &AtomicBool,
) -> Result<Option<u64>, QuarryError> {
    count_records_with_progress(source, delimiter, cancel, |_| {})
}

#[cfg(test)]
pub(crate) fn count_records_with_progress(
    source: &mut File,
    delimiter: u8,
    cancel: &AtomicBool,
    progress: impl FnMut(u64),
) -> Result<Option<u64>, QuarryError> {
    count_records_encoded(source, delimiter, crate::Encoding::Utf8, cancel, progress)
}

pub(crate) fn count_records_encoded(
    source: &mut File,
    delimiter: u8,
    encoding: crate::Encoding,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<Option<u64>, QuarryError> {
    source.seek(SeekFrom::Start(0))?;
    let mut scanner = crate::encoding::RecordScanner::new(delimiter, encoding)?;
    let mut chunk = vec![0; encoding.chunk_size(crate::DEFAULT_READ_CHUNK)];
    let mut offset = 0;
    let mut records = 0_u64;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(None);
        }
        let read = encoding.read_chunk(source, &mut chunk)?;
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

/// Restrict a newly created private file to owner-only permissions before writing sensitive
/// data (POSIX `0600` + empty ACL on macOS, protected owner-only DACL on Windows).
pub fn restrict_private_file(path: &Path, file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = path;
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        #[cfg(target_os = "macos")]
        set_macos_acl(file, None)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        const READ_CONTROL: u32 = 0x0002_0000;
        const WRITE_DAC: u32 = 0x0004_0000;
        let expected = crate::SourceStamp::from_file(file)?;
        let acl_handle = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let meta = acl_handle.metadata()?;
        if !meta.is_file() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "private file cannot be a reparse point",
            ));
        }
        let observed = crate::SourceStamp::from_file(&acl_handle)?;
        if !expected.same_identity(&observed) {
            return Err(io::Error::other(
                "private file changed while restricting permissions",
            ));
        }
        crate::sidecar::set_windows_owner_only_dacl(acl_handle.as_raw_handle())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, file);
        Ok(())
    }
}

/// Identifies filesystems that cannot store extended ACLs.
#[cfg(target_os = "macos")]
fn unsupported_acl(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::ENOTSUP || code == libc::EOPNOTSUPP
    )
}

/// Uses private POSIX modes when ACLs are unavailable; never masks access errors.
#[cfg(target_os = "macos")]
fn validate_acl_update(file: &File, private: bool, result: io::Result<()>) -> io::Result<()> {
    match result {
        Ok(()) => {}
        Err(error) if unsupported_acl(&error) => {}
        Err(error) => return Err(error),
    }
    if private && file.metadata()?.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "filesystem cannot enforce private file permissions",
        ));
    }
    Ok(())
}

/// Clears staging ACLs, or restores the existing destination ACL just before publication.
/// Open descriptors keep both operations bound to the intended filesystem objects.
#[cfg(target_os = "macos")]
pub(crate) fn set_macos_acl(file: &File, source: Option<&File>) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn acl_init(count: libc::c_int) -> *mut libc::c_void;
        fn acl_get_fd(fd: libc::c_int) -> *mut libc::c_void;
        fn acl_set_fd(fd: libc::c_int, acl: *mut libc::c_void) -> libc::c_int;
        fn acl_free(object: *mut libc::c_void) -> libc::c_int;
    }

    // SAFETY: source descriptors are live; otherwise zero requests an initialized
    // empty ACL. Both functions return owned ACL storage and null is checked.
    let mut acl = unsafe {
        match source {
            Some(source) => acl_get_fd(source.as_raw_fd()),
            None => acl_init(0),
        }
    };
    if acl.is_null() && source.is_some() {
        let error = io::Error::last_os_error();
        // ENOENT means no extended ACL; unsupported filesystems have no ACL to copy.
        if error.raw_os_error() != Some(libc::ENOENT) && !unsupported_acl(&error) {
            return Err(error);
        }
        // SAFETY: no source ACL is equivalent to a new, empty ACL.
        acl = unsafe { acl_init(0) };
    }
    if acl.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor is live and the ACL remains allocated for this call.
    let status = unsafe { acl_set_fd(file.as_raw_fd(), acl) };
    let result = if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    // SAFETY: acl_init or acl_get_fd allocated this object; it has not been freed.
    unsafe { acl_free(acl) };
    validate_acl_update(file, source.is_none(), result)
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
                DuplicateSpec::columns(vec![0], CaseSensitivity::Sensitive),
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
            ColumnTransformation::Cleanup(crate::ColumnCleanup {
                columns: vec![0, 1, 2],
                operation: crate::CleanupOperation::Trim,
            }),
            ColumnTransformation::Cleanup(crate::ColumnCleanup {
                columns: vec![0, 1, 2],
                operation: crate::CleanupOperation::CollapseSpaces,
            }),
            ColumnTransformation::Cleanup(crate::ColumnCleanup {
                columns: vec![0, 1, 2],
                operation: crate::CleanupOperation::Uppercase,
            }),
            ColumnTransformation::Cleanup(crate::ColumnCleanup {
                columns: vec![0, 1, 2],
                operation: crate::CleanupOperation::Lowercase,
            }),
            ColumnTransformation::Cleanup(crate::ColumnCleanup {
                columns: vec![0, 1, 2],
                operation: crate::CleanupOperation::FillEmpty(b"filled,\"quoted\"\nvalue".to_vec()),
            }),
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

    #[test]
    fn borrowed_overlay_estimates_preserve_bounds_without_owned_snapshots() {
        let headers = BTreeMap::from([(0, b"x".to_vec())]);
        let cells = BTreeMap::from([
            ((1, 0), b"ab".to_vec()),
            ((1, 1), b"c".to_vec()),
            ((3, 0), b"d".to_vec()),
        ]);
        let borrowed = super::estimate_edited_output_bytes_from_values(
            100,
            0,
            headers.iter().map(|(key, value)| (*key, value.as_slice())),
            cells.iter().map(|(key, value)| (*key, value.as_slice())),
            None,
            None,
        );
        // (100 source + 18 overlay + 6 row-quote allowance) * 2 + 3 BOM allowance.
        assert_eq!(borrowed, 251);
        assert_eq!(
            super::estimate_edited_output_bytes(100, 0, &headers, &cells, None, None),
            251
        );
        assert_eq!(
            super::estimate_edited_output_bytes_from_values(
                u64::MAX,
                0,
                std::iter::empty(),
                std::iter::empty(),
                None,
                None,
            ),
            u64::MAX
        );
    }
    /// Unsupported ACLs are safe only with private POSIX modes; other failures remain errors.
    #[cfg(target_os = "macos")]
    #[test]
    fn unsupported_acl_requires_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private.csv");
        std::fs::write(&path, b"synthetic").unwrap();
        let file = File::open(&path).unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .unwrap();
        for code in [libc::ENOTSUP, libc::EOPNOTSUPP] {
            assert!(
                validate_acl_update(&file, true, Err(io::Error::from_raw_os_error(code))).is_ok()
            );
        }
        assert!(
            validate_acl_update(&file, true, Err(io::Error::from_raw_os_error(libc::EPERM)))
                .is_err()
        );
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(
            validate_acl_update(
                &file,
                true,
                Err(io::Error::from_raw_os_error(libc::ENOTSUP))
            )
            .is_err()
        );
        assert!(
            validate_acl_update(
                &file,
                false,
                Err(io::Error::from_raw_os_error(libc::ENOTSUP))
            )
            .is_ok()
        );
        assert!(validate_acl_update(&file, true, Ok(())).is_err());
    }

    #[test]
    fn validate_scratch_directory_accepts_user_dir_and_rejects_invalid_targets() {
        let dir = tempfile::tempdir().unwrap();
        let space = super::validate_scratch_directory(dir.path()).unwrap();
        assert_eq!(space.directory, dir.path());

        let missing = dir.path().join("missing-drive");
        assert!(super::validate_scratch_directory(&missing).is_err());

        let regular = dir.path().join("file.txt");
        fs::write(&regular, b"x").unwrap();
        assert!(super::validate_scratch_directory(&regular).is_err());

        assert!(super::validate_scratch_directory(Path::new("")).is_err());
        assert!(super::validate_scratch_directory(Path::new("relative/scratch")).is_err());

        #[cfg(unix)]
        {
            let target = dir.path().join("real");
            fs::create_dir(&target).unwrap();
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(super::validate_scratch_directory(&link).is_err());
            let nested = target.join("child");
            fs::create_dir(&nested).unwrap();
            assert!(super::validate_scratch_directory(&link.join("child")).is_err());
            assert!(super::validate_scratch_directory(&nested).is_ok());
        }
    }
}
