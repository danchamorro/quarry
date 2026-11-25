//! Guarded Section Splice-Save and section-scoped working-copy rewrites (ADR 0015 / `T13`).
//!
//! Strictly requires `BoundaryTrust::Verified` before mutating or splicing a bounded section back
//! into its parent file. Uses Copy-on-Write whole-file cloning (`fclonefileat` on macOS APFS,
//! `FICLONE` on Linux) with a second intact CoW snapshot clone for length-changing
//! edits, falling back cleanly to bounded streaming rewrites on non-CoW filesystems.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::ops::RangeInclusive;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;

use quarry_delimited::parse_record;

use crate::case::ByteMatcher;
use crate::export::{
    ExportConfig, ExportTarget, LiteralReplacement, ReplaceAllJob, RowInsertion, SaveAsJob,
    SaveAsSummary, SaveWorkerOutcome, SharedState, UTF8_BOM, WorkerCompletion,
    private_staging_directory, source_matches_stamp, write_serialized_fields,
};
use crate::positional_io::read_at_exact_or_eof;
use crate::section::{BoundaryTrust, ByteWindow, SectionBounds};
#[cfg(target_os = "macos")]
use crate::storage::set_macos_acl;
use crate::storage::{available_disk_space, check_storage, output_parent, storage_error};
use crate::{Encoding, QuarryError};

const SPLICE_COPY_CHUNK: usize = 256 * 1024;

/// Strategy selected by the guarded section splice-save worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpliceStrategy {
    /// Same-length edit written directly into a whole-file CoW clone (`fclonefileat` / `FICLONE`).
    CowInPlace,
    /// Length-changing edit using a CoW output clone plus a second intact CoW snapshot clone for
    /// consistent suffix streaming.
    CowSnapshotTruncate,
    /// Portable streaming prefix + section + suffix rewrite on non-CoW filesystems.
    StreamingRewrite,
}

/// Advisory disk preflight estimate for a guarded section splice-save.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpliceDiskPreflight {
    pub available_bytes: u64,
    pub estimated_required_bytes: u64,
    pub uses_cow: bool,
}

/// Completion summary returned inside [`SaveAsSummary::splice`] after a guarded section splice-save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpliceSaveSummary {
    pub destination: PathBuf,
    pub new_bounds: SectionBounds,
    pub new_file_size: u64,
    pub rows_written: u64,
    pub section_bytes_written: u64,
    pub total_bytes_written: u64,
    pub strategy: SpliceStrategy,
    /// Guard captured from the exact published file descriptor right after `fsync` and before
    /// atomic rename, so post-save reload binds `new_bounds` only to this generation.
    pub published_guard: crate::SourceGuard,
}

/// Section-scoped working-copy mutation kind.
pub(crate) enum SectionWorkingOp {
    InsertRows(RowInsertion),
    DeleteRows(Vec<RangeInclusive<u64>>),
    ReplaceAll(LiteralReplacement),
}

/// Attempt a whole-file Copy-on-Write clone of `src` to `dst_dir/dst_name` (restricted to `0600`
/// during staging). Returns `Ok(None)` when the filesystem does not support reflinks so callers
/// fall back cleanly to streaming rewrites.
#[cfg(target_os = "macos")]
fn cow_clone_file(src: &File, dst_dir: &Path, dst_name: &str) -> io::Result<Option<File>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;

    let dir_c = CString::new(dst_dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "staging dir contains NUL"))?;
    let name_c = CString::new(dst_name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "staging name contains NUL"))?;
    let dir_fd = unsafe {
        libc::open(
            dir_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if dir_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let clone_ret = unsafe { libc::fclonefileat(src.as_raw_fd(), dir_fd, name_c.as_ptr(), 0) };
    let clone_err = if clone_ret != 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        libc::close(dir_fd);
    }
    if let Some(err) = clone_err {
        return match err.raw_os_error() {
            Some(
                libc::ENOTSUP
                | libc::EXDEV
                | libc::ENOSYS
                | libc::EINVAL
                | libc::EACCES
                | libc::EPERM,
            ) => Ok(None),
            _ => Err(err),
        };
    }
    let dst_path = dst_dir.join(dst_name);
    // `fclonefileat` preserves the source file's mode (e.g. `0444`), so open read-only first,
    // chmod the clone descriptor to `0600` to make it writable during staging, then reopen R/W.
    // Fall back cleanly to streaming if permissions prevent modifying or opening the clone.
    let ro_file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&dst_path)
    {
        Ok(file) => file,
        Err(err) => {
            let _ = fs::remove_file(&dst_path);
            return if err.kind() == io::ErrorKind::PermissionDenied {
                Ok(None)
            } else {
                Err(err)
            };
        }
    };
    if let Err(err) = ro_file.set_permissions(fs::Permissions::from_mode(0o600)) {
        drop(ro_file);
        let _ = fs::remove_file(&dst_path);
        return if err.kind() == io::ErrorKind::PermissionDenied {
            Ok(None)
        } else {
            Err(err)
        };
    }
    drop(ro_file);
    match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&dst_path)
    {
        Ok(file) => Ok(Some(file)),
        Err(err) => {
            let _ = fs::remove_file(&dst_path);
            if err.kind() == io::ErrorKind::PermissionDenied {
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn cow_clone_file(src: &File, dst_dir: &Path, dst_name: &str) -> io::Result<Option<File>> {
    let dst_path = dst_dir.join(dst_name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&dst_path)?;
    match rustix::fs::ioctl_ficlone(&file, src) {
        Ok(()) => Ok(Some(file)),
        Err(errno) => {
            drop(file);
            let _ = fs::remove_file(&dst_path);
            let io_err = io::Error::from_raw_os_error(errno.raw_os_error());
            match errno {
                rustix::io::Errno::OPNOTSUPP
                | rustix::io::Errno::XDEV
                | rustix::io::Errno::INVAL
                | rustix::io::Errno::NOSYS
                | rustix::io::Errno::BADF
                | rustix::io::Errno::TXTBSY
                | rustix::io::Errno::PERM
                | rustix::io::Errno::ACCESS => Ok(None),
                _ => Err(io_err),
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn cow_clone_file(_src: &File, _dst_dir: &Path, _dst_name: &str) -> io::Result<Option<File>> {
    Ok(None)
}

/// Probe whether `parent_path`'s volume supports CoW cloning without leaving any artifacts on disk.
pub(crate) fn probe_cow_support(parent_path: &Path) -> bool {
    let Ok(parent_file) = crate::source_stamp::open_regular_file_nofollow(parent_path) else {
        return false;
    };
    let parent_dir = output_parent(parent_path);
    let Ok(dir) = private_staging_directory(parent_dir) else {
        return false;
    };
    matches!(
        cow_clone_file(&parent_file, dir.path(), "probe.stage"),
        Ok(Some(_))
    )
}

/// Peak temporary disk space needed on the parent's volume during a splice-save.
///
/// `section.stage` (`section_bytes`) coexists with `output.stage` while writing the section
/// payload and is dropped + unlinked before suffix streaming begins.
fn splice_peak_required_bytes(
    uses_cow: bool,
    aligned_start: u64,
    section_bytes: u64,
    suffix_len: u64,
) -> u64 {
    if uses_cow {
        section_bytes.saturating_add(section_bytes.max(suffix_len))
    } else {
        aligned_start
            .saturating_add(section_bytes)
            .saturating_add(section_bytes.max(suffix_len))
    }
}

/// Estimate advisory disk preflight for splicing a `Verified` section back into `parent_path`.
pub(crate) fn estimate_splice_preflight(
    parent_path: &Path,
    parent_file_size: u64,
    aligned_start: u64,
    aligned_end: u64,
    section_source_len: u64,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
) -> Result<SpliceDiskPreflight, QuarryError> {
    let parent_dir = output_parent(parent_path);
    let available_bytes = available_disk_space(parent_dir)?;
    let uses_cow = probe_cow_support(parent_path);
    let section_est = crate::estimate_edited_output_bytes_from_values(
        section_source_len,
        0,
        std::iter::empty(),
        cell_edits.iter().map(|(&k, v)| (k, v.as_slice())),
        None,
        None,
    );
    let suffix_len = parent_file_size.saturating_sub(aligned_end);
    let old_section_len = aligned_end.saturating_sub(aligned_start);
    let estimated_required_bytes = if uses_cow && section_est == old_section_len {
        // Same-length CoW splice (`CowInPlace`) only writes the section blocks without streaming
        // the suffix.
        section_est.saturating_mul(2)
    } else {
        splice_peak_required_bytes(uses_cow, aligned_start, section_est, suffix_len)
    };
    Ok(SpliceDiskPreflight {
        available_bytes,
        estimated_required_bytes,
        uses_cow,
    })
}

fn detect_encoded_line_ending(
    file: &File,
    read_base: u64,
    read_end: u64,
    delimiter: u8,
    encoding: Encoding,
) -> Result<&'static [u8], QuarryError> {
    if read_base >= read_end {
        return Ok(b"\n");
    }
    const MAX_PROBE_BYTES: u64 = 256 * 1024;
    let probe_end = read_end.min(read_base.saturating_add(MAX_PROBE_BYTES));
    let mut scanner = crate::encoding::RecordScanner::at_offset(delimiter, read_base, encoding)?;
    let mut chunk = vec![0u8; encoding.chunk_size(64 * 1024)];
    let mut cursor = read_base;
    let mut first_record_end: Option<u64> = None;

    while cursor < probe_end && first_record_end.is_none() {
        let remaining = probe_end - cursor;
        let mut limit = chunk
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        if encoding.is_utf16() {
            limit &= !1;
        }
        if limit == 0 {
            break;
        }
        let read = read_at_exact_or_eof(file, cursor, &mut chunk[..limit])?;
        if read == 0 {
            break;
        }
        scanner.scan_chunk(&chunk[..read], cursor, |abs_end| {
            if first_record_end.is_none() {
                first_record_end = Some(abs_end);
            }
        })?;
        cursor += read as u64;
    }

    let Some(end) = first_record_end else {
        return Ok(b"\n");
    };
    let tail_len = if encoding.is_utf16() { 4u64 } else { 2u64 };
    if end.saturating_sub(read_base) < tail_len {
        return Ok(b"\n");
    }
    let mut tail = [0u8; 4];
    let read = read_at_exact_or_eof(file, end - tail_len, &mut tail[..tail_len as usize])?;
    if read < tail_len as usize {
        return Ok(b"\n");
    }
    match encoding {
        Encoding::Utf16Le if tail == [b'\r', 0, b'\n', 0] => Ok(b"\r\n"),
        Encoding::Utf16Be if tail == [0, b'\r', 0, b'\n'] => Ok(b"\r\n"),
        Encoding::Utf8 | Encoding::Windows1252 | Encoding::Latin1 if &tail[..2] == b"\r\n" => {
            Ok(b"\r\n")
        }
        _ => Ok(b"\n"),
    }
}

fn encode_utf8_bytes(
    bytes: &[u8],
    encoding: Encoding,
    max_record_bytes: usize,
) -> Result<Vec<u8>, QuarryError> {
    encoding
        .encode(bytes, max_record_bytes)
        .map(|cow| cow.into_owned())
}

fn serialize_utf8_fields_record(
    prefix: &[u8],
    fields: &[Vec<u8>],
    delimiter: u8,
    ending: &[u8],
    is_first_output_record: bool,
    max_record_bytes: usize,
) -> Result<Vec<u8>, QuarryError> {
    let mut out = Vec::with_capacity(prefix.len() + ending.len() + fields.len() * 8);
    out.extend_from_slice(prefix);
    for (idx, field) in fields.iter().enumerate() {
        if idx > 0 {
            out.push(delimiter);
        }
        let force_quotes = (is_first_output_record
            && prefix.is_empty()
            && idx == 0
            && field.starts_with(UTF8_BOM))
            || (fields.len() == 1 && field.is_empty() && ending.is_empty());
        let needs_quotes = force_quotes
            || field
                .iter()
                .any(|&b| b == delimiter || b == b'"' || b == b'\n' || b == b'\r');
        if needs_quotes {
            out.push(b'"');
            for &b in field {
                if b == b'"' {
                    out.push(b'"');
                }
                out.push(b);
            }
            out.push(b'"');
        } else {
            out.extend_from_slice(field);
        }
    }
    out.extend_from_slice(ending);
    if out.len() > max_record_bytes {
        return Err(QuarryError::RecordTooLarge {
            limit: max_record_bytes,
        });
    }
    Ok(out)
}

fn raw_record_has_lf(raw: &[u8], encoding: Encoding) -> bool {
    match encoding {
        Encoding::Utf16Le => {
            raw.len() >= 2 && raw[raw.len() - 2] == b'\n' && raw[raw.len() - 1] == 0
        }
        Encoding::Utf16Be => {
            raw.len() >= 2 && raw[raw.len() - 2] == 0 && raw[raw.len() - 1] == b'\n'
        }
        _ => raw.last() == Some(&b'\n'),
    }
}

/// Serialize `[read_base, read_end)` from `source` applying sparse `cell_edits` into `writer`.
/// Untouched rows are copied raw byte-for-byte; edited rows preserve original line endings and
/// encode back into `encoding` without injecting headers or interior BOMs.
#[allow(clippy::too_many_arguments)]
fn serialize_section_to_stage(
    source: &mut File,
    read_base: u64,
    read_end: u64,
    section_is_interior: bool,
    source_had_bom: bool,
    has_trailing_suffix: bool,
    delimiter: u8,
    encoding: Encoding,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    config: ExportConfig,
    writer: &mut BufWriter<File>,
    shared: &SharedState,
) -> Result<Option<(u64, u64)>, QuarryError> {
    let mut bytes_written = 0u64;
    let bom = encoding.bom();
    let scan_start = if !section_is_interior && source_had_bom && !bom.is_empty() {
        let bom_len = bom.len() as u64;
        if read_end.saturating_sub(read_base) >= bom_len {
            let mut probe = [0u8; 3];
            let read = read_at_exact_or_eof(source, read_base, &mut probe[..bom.len()])?;
            if read == bom.len() && &probe[..bom.len()] == bom {
                writer.write_all(bom)?;
                bytes_written += bom_len;
                shared.bytes_written.store(bytes_written, Ordering::Release);
                read_base + bom_len
            } else {
                read_base
            }
        } else {
            read_base
        }
    } else {
        read_base
    };
    let wrote_leading_bom = scan_start > read_base;
    let default_ending =
        detect_encoded_line_ending(source, scan_start, read_end, delimiter, encoding)?;
    if encoding.is_utf16() {
        source.seek(SeekFrom::Start(scan_start))?;
    }
    let mut scanner = crate::encoding::RecordScanner::at_offset(delimiter, scan_start, encoding)?;
    let mut chunk = vec![0u8; encoding.chunk_size(config.chunk_bytes)];
    let mut cursor = scan_start;
    let mut local_row = 0u64;
    let mut rows_written = 0u64;
    let mut record = Vec::new();
    let mut last_record_had_lf = true;
    let mut next_cell_edit_row = cell_edits.first_key_value().map(|((row, _), _)| *row);

    let mut emit_record = |raw: &[u8], row_idx: u64| -> Result<(), QuarryError> {
        let has_cell_edit = next_cell_edit_row == Some(row_idx);
        if has_cell_edit {
            next_cell_edit_row = row_idx.checked_add(1).and_then(|next_row| {
                cell_edits
                    .range((next_row, 0)..)
                    .next()
                    .map(|((row, _), _)| *row)
            });
        }
        if has_cell_edit {
            let decoded = encoding
                .decode(raw, config.max_record_bytes)
                .map_err(|error| crate::record_encoding_error(row_idx.saturating_add(1), error))?;
            let body = decoded.as_ref();
            let ending = if body.ends_with(b"\r\n") {
                b"\r\n".as_slice()
            } else if body.ends_with(b"\n") {
                b"\n".as_slice()
            } else if has_trailing_suffix {
                default_ending
            } else {
                b"".as_slice()
            };
            let mut fields = parse_record(body, delimiter)?
                .into_iter()
                .map(|field| field.into_owned())
                .collect::<Vec<_>>();
            let row_edits = cell_edits.range((row_idx, 0)..=(row_idx, usize::MAX));
            if row_edits
                .clone()
                .next_back()
                .is_some_and(|((_, col), _)| *col >= fields.len())
            {
                return Err(QuarryError::InvalidOption(
                    "cell edit column is out of range",
                ));
            }
            for ((_, col), value) in row_edits {
                fields[*col] = value.clone();
            }
            let is_first_output_record = !section_is_interior && !wrote_leading_bom && row_idx == 0;
            let utf8_rec = serialize_utf8_fields_record(
                &[],
                &fields,
                delimiter,
                ending,
                is_first_output_record,
                config.max_record_bytes,
            )?;
            let encoded = encode_utf8_bytes(&utf8_rec, encoding, config.max_record_bytes)?;
            if encoded.len() > config.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
            writer.write_all(&encoded)?;
            bytes_written += encoded.len() as u64;
            last_record_had_lf = !ending.is_empty();
        } else {
            writer.write_all(raw)?;
            bytes_written += raw.len() as u64;
            last_record_had_lf = raw_record_has_lf(raw, encoding);
        }
        rows_written += 1;
        shared.bytes_written.store(bytes_written, Ordering::Release);
        Ok(())
    };

    while cursor < read_end {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        let remaining = read_end - cursor;
        let limit = chunk
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let read = if encoding.is_utf16() {
            encoding.read_chunk(source, &mut chunk[..limit])?
        } else {
            read_at_exact_or_eof(source, cursor, &mut chunk[..limit])?
        };
        if read == 0 {
            return Err(QuarryError::SourceChanged);
        }
        let mut seg_start = 0usize;
        let mut deferred_error = None;
        let mut cancelled = false;
        let scan_res = scanner.scan_chunk(&chunk[..read], cursor, |abs_end| {
            let local_end = (abs_end - cursor) as usize;
            if deferred_error.is_none() && !cancelled {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    cancelled = true;
                } else {
                    let slice = &chunk[seg_start..local_end];
                    let total_len = record.len().saturating_add(slice.len());
                    if total_len > config.max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: config.max_record_bytes,
                        });
                    } else {
                        let rec: &[u8] = if record.is_empty() {
                            slice
                        } else {
                            record.extend_from_slice(slice);
                            &record
                        };
                        if let Err(err) = emit_record(rec, local_row) {
                            deferred_error = Some(err);
                        }
                    }
                }
            }
            record.clear();
            local_row += 1;
            seg_start = local_end;
        });
        if cancelled {
            return Ok(None);
        }
        if let Some(error) = deferred_error {
            return Err(error);
        }
        scan_res?;
        if seg_start < read {
            let tail = &chunk[seg_start..read];
            if record.len().saturating_add(tail.len()) > config.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
            record.extend_from_slice(tail);
        }
        cursor += read as u64;
        shared
            .bytes_scanned
            .store(cursor.saturating_sub(read_base), Ordering::Release);
    }

    let mut finish_err = None;
    let mut finish_cancelled = false;
    scanner.finish(read_end, |_| {
        if shared.cancel_requested.load(Ordering::Acquire) {
            finish_cancelled = true;
            return;
        }
        if !record.is_empty() {
            if let Err(err) = emit_record(&record, local_row) {
                finish_err = Some(err);
            }
            local_row += 1;
            record.clear();
        }
    })?;
    if finish_cancelled {
        return Ok(None);
    }
    if let Some(error) = finish_err {
        return Err(error);
    }
    if let Some(((last_edit_row, _), _)) = cell_edits.last_key_value()
        && *last_edit_row >= local_row
    {
        return Err(QuarryError::InvalidOption("cell edit row is out of range"));
    }
    if has_trailing_suffix && rows_written > 0 && !last_record_had_lf {
        let encoded_ending = encode_utf8_bytes(default_ending, encoding, config.max_record_bytes)?;
        writer.write_all(&encoded_ending)?;
        bytes_written += encoded_ending.len() as u64;
        shared.bytes_written.store(bytes_written, Ordering::Release);
    }
    writer.flush()?;
    Ok(Some((rows_written, bytes_written)))
}

fn copy_file_range_streaming(
    src: &File,
    mut src_offset: u64,
    mut remaining: u64,
    dst: &mut File,
    cancel_requested: &std::sync::atomic::AtomicBool,
    bytes_written_counter: Option<&std::sync::atomic::AtomicU64>,
    parent_dir: &Path,
) -> Result<Option<u64>, QuarryError> {
    let mut buf = vec![0u8; SPLICE_COPY_CHUNK];
    let mut copied = 0u64;
    while remaining > 0 {
        if cancel_requested.load(Ordering::Acquire) {
            return Ok(None);
        }
        let want = remaining.min(buf.len() as u64) as usize;
        let read = read_at_exact_or_eof(src, src_offset, &mut buf[..want])?;
        if read < want {
            return Err(QuarryError::SourceChanged);
        }
        dst.write_all(&buf[..read])
            .map_err(|error| storage_error(parent_dir, error))?;
        src_offset += read as u64;
        remaining -= read as u64;
        copied += read as u64;
        if let Some(counter) = bytes_written_counter {
            counter.fetch_add(read as u64, Ordering::Release);
        }
    }
    Ok(Some(copied))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn start_section_splice(
    parent_path: PathBuf,
    parent_stamp: crate::SourceStamp,
    parent_file_size: u64,
    parent_bounds: SectionBounds,
    source_had_bom: bool,
    section_source_path: PathBuf,
    section_source_stamp: crate::SourceStamp,
    read_base: u64,
    read_end: u64,
    delimiter: u8,
    has_header: bool,
    header_renames: BTreeMap<usize, Vec<u8>>,
    cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
    config: ExportConfig,
) -> Result<SaveAsJob, QuarryError> {
    if parent_bounds.trust != BoundaryTrust::Verified {
        return Err(QuarryError::InvalidOption(
            "Verify section boundaries before saving changes back to the parent file.",
        ));
    }
    if !header_renames.is_empty() {
        return Err(QuarryError::InvalidOption(
            "Header edits cannot be splice-saved into the parent file. Use Save Section As New instead.",
        ));
    }
    if config.chunk_bytes == 0 {
        return Err(QuarryError::InvalidOption("splice chunk must be non-zero"));
    }
    let aligned_start = parent_bounds.snapped.base;
    let aligned_end = parent_bounds.snapped_end();
    if aligned_end > parent_file_size {
        return Err(QuarryError::SourceChanged);
    }
    let section_is_interior = aligned_start > 0;
    let data_start = u64::from(has_header && !section_is_interior);
    if cell_edits
        .first_key_value()
        .is_some_and(|((row, _), _)| *row < data_start)
    {
        return Err(QuarryError::InvalidOption(
            "cell edits must target data rows",
        ));
    }
    for value in cell_edits.values() {
        if value.len() > config.max_record_bytes {
            return Err(QuarryError::RecordTooLarge {
                limit: config.max_record_bytes,
            });
        }
    }

    let parent_meta = fs::symlink_metadata(&parent_path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            QuarryError::SourceChanged
        } else {
            error.into()
        }
    })?;
    if parent_meta.file_type().is_symlink() {
        return Err(QuarryError::InvalidOption(
            "saving through a symbolic link is not supported; use Save As instead",
        ));
    }
    let parent_file =
        crate::source_stamp::open_regular_file_nofollow(&parent_path).map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
            ) {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
    if !source_matches_stamp(&parent_file, &parent_path, &parent_stamp)? {
        return Err(QuarryError::SourceChanged);
    }

    let is_working_copy = section_source_path != parent_path;
    let section_file = if is_working_copy {
        let f = crate::source_stamp::open_regular_file_nofollow(&section_source_path).map_err(
            |error| {
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                ) {
                    QuarryError::SourceChanged
                } else {
                    error.into()
                }
            },
        )?;
        if !source_matches_stamp(&f, &section_source_path, &section_source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        f
    } else {
        parent_file.try_clone()?
    };

    let section_len = read_end.saturating_sub(read_base);
    let shared = Arc::new(SharedState::new(section_len.max(1)));
    let worker_state = Arc::clone(&shared);
    let handle = thread::Builder::new()
        .name("quarry-splice-save".into())
        .spawn(move || {
            let _completion = WorkerCompletion(&worker_state);
            let result = run_section_splice(
                parent_path,
                parent_file,
                parent_stamp,
                parent_file_size,
                parent_bounds,
                source_had_bom,
                section_source_path,
                section_file,
                section_source_stamp,
                is_working_copy,
                read_base,
                read_end,
                delimiter,
                &cell_edits,
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
    Ok(SaveAsJob::from_worker(None, shared, handle))
}

#[allow(clippy::too_many_arguments)]
fn run_section_splice(
    parent_path: PathBuf,
    parent_file: File,
    parent_stamp: crate::SourceStamp,
    parent_file_size: u64,
    parent_bounds: SectionBounds,
    source_had_bom: bool,
    section_source_path: PathBuf,
    mut section_file: File,
    section_source_stamp: crate::SourceStamp,
    is_working_copy: bool,
    read_base: u64,
    read_end: u64,
    delimiter: u8,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    config: ExportConfig,
    shared: &SharedState,
) -> Result<SaveWorkerOutcome, QuarryError> {
    if shared.cancel_requested.load(Ordering::Acquire) {
        return Ok(SaveWorkerOutcome::Cancelled);
    }
    let aligned_start = parent_bounds.snapped.base;
    let aligned_end = parent_bounds.snapped_end();
    let old_section_len = aligned_end.saturating_sub(aligned_start);
    let suffix_len = parent_file_size.saturating_sub(aligned_end);
    let parent_dir = output_parent(&parent_path);
    let parent_permissions = parent_file.metadata()?.permissions();

    // All staging files live inside this private 0700 directory on the parent's volume so RAII
    // drop cleans up output, snapshot, and section staging on any error or cancellation.
    let staging_dir =
        private_staging_directory(parent_dir).map_err(|error| storage_error(parent_dir, error))?;
    let output_stage_path = staging_dir.path().join("output.stage");
    let section_stage_path = staging_dir.path().join("section.stage");
    let snapshot_stage_path = staging_dir.path().join("snapshot.stage");

    // 1. Determine CoW capability directly on the parent's volume.
    let mut cow_output = cow_clone_file(&parent_file, staging_dir.path(), "output.stage")
        .map_err(|error| storage_error(parent_dir, error))?;
    let cow_supported = cow_output.is_some();

    // 2. Check the section-stage allowance before serialization. Do NOT budget for suffix copying
    //    yet because a same-length CoW edit (`CowInPlace`) only writes the section blocks.
    let section_est = crate::estimate_edited_output_bytes_from_values(
        read_end.saturating_sub(read_base),
        0,
        std::iter::empty(),
        cell_edits.iter().map(|(&k, v)| (k, v.as_slice())),
        None,
        None,
    );
    shared
        .estimated_output_bytes
        .store(section_est, Ordering::Release);
    check_storage(parent_dir, section_est)?;

    if shared.cancel_requested.load(Ordering::Acquire) {
        return Ok(SaveWorkerOutcome::Cancelled);
    }

    // 3. Serialize edited section span into `section.stage` to obtain exact `new_section_len`.
    let section_stage_file = {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        opts.open(&section_stage_path)
            .map_err(|error| storage_error(parent_dir, error))?
    };
    let mut section_writer = BufWriter::new(section_stage_file);
    let Some((rows_written, new_section_len)) = serialize_section_to_stage(
        &mut section_file,
        read_base,
        read_end,
        aligned_start > 0,
        source_had_bom,
        aligned_end < parent_file_size,
        delimiter,
        config.source_encoding,
        cell_edits,
        config,
        &mut section_writer,
        shared,
    )?
    else {
        return Ok(SaveWorkerOutcome::Cancelled);
    };
    let section_stage_file = section_writer
        .into_inner()
        .map_err(|error| storage_error(parent_dir, error.into_error()))?;

    let new_file_size = aligned_start
        .checked_add(new_section_len)
        .and_then(|v| v.checked_add(suffix_len))
        .ok_or(QuarryError::InvalidOption(
            "spliced file size overflows 64-bit byte range",
        ))?;

    // Guard against promoting a surviving suffix row that begins with a literal U+FEFF (BOM)
    // to byte 0 of a BOM-less file when every row in a byte-0 section was deleted.
    if aligned_start == 0 && !source_had_bom && new_section_len == 0 && suffix_len > 0 {
        let bom = config.source_encoding.bom();
        if !bom.is_empty() && suffix_len >= bom.len() as u64 {
            let mut probe = [0u8; 3];
            let read = read_at_exact_or_eof(&parent_file, aligned_end, &mut probe[..bom.len()])?;
            if read < bom.len() {
                return Err(QuarryError::SourceChanged);
            }
            if &probe[..bom.len()] == bom {
                return Err(QuarryError::InvalidOption(
                    "Deleting every row in this byte-0 section would promote a surviving row starting with a literal U+FEFF (BOM) to byte 0 of a BOM-less file. Keep at least one row or use Save Section As New.",
                ));
            }
        }
    }

    // Revalidate remaining disk headroom using the exact materialized `new_section_len` and
    // selected strategy now that `section.stage` is on disk and before allocating `output.stage`.
    let remaining_required = if cow_supported {
        if new_section_len == old_section_len {
            new_section_len
        } else {
            new_section_len.max(suffix_len)
        }
    } else {
        aligned_start.saturating_add(new_section_len.max(suffix_len))
    };
    shared.estimated_output_bytes.store(
        new_section_len.saturating_add(remaining_required),
        Ordering::Release,
    );
    check_storage(parent_dir, remaining_required)?;

    // 4. Assemble `output.stage` using CoW (in-place or consistent-snapshot clone) or streaming.
    let (output_file, strategy, total_bytes_written) = if let Some(mut output_file) =
        cow_output.take()
    {
        if new_section_len == old_section_len {
            // Same-length CoW splice: overwrite only `[aligned_start, aligned_end)` in place!
            output_file
                .seek(SeekFrom::Start(aligned_start))
                .map_err(|error| storage_error(parent_dir, error))?;
            let Some(written) = copy_file_range_streaming(
                &section_stage_file,
                0,
                new_section_len,
                &mut output_file,
                &shared.cancel_requested,
                None,
                parent_dir,
            )?
            else {
                return Ok(SaveWorkerOutcome::Cancelled);
            };
            drop(section_stage_file);
            let _ = fs::remove_file(&section_stage_path);
            (output_file, SpliceStrategy::CowInPlace, written)
        } else {
            // Length-changing CoW splice: take a SECOND intact zero-copy CoW snapshot clone of
            // the parent file BEFORE `ftruncate` so suffix reads never see partial/external writes.
            let snapshot_opt = cow_clone_file(&parent_file, staging_dir.path(), "snapshot.stage")
                .map_err(|error| storage_error(parent_dir, error))?;
            if let Some(snapshot_file) = snapshot_opt {
                let snapshot_len = snapshot_file.metadata()?.len();
                if snapshot_len != parent_file_size {
                    return Err(QuarryError::SourceChanged);
                }
                output_file
                    .set_len(aligned_start)
                    .map_err(|error| storage_error(parent_dir, error))?;
                output_file
                    .seek(SeekFrom::Start(aligned_start))
                    .map_err(|error| storage_error(parent_dir, error))?;
                let Some(sec_written) = copy_file_range_streaming(
                    &section_stage_file,
                    0,
                    new_section_len,
                    &mut output_file,
                    &shared.cancel_requested,
                    None,
                    parent_dir,
                )?
                else {
                    return Ok(SaveWorkerOutcome::Cancelled);
                };
                // Free `section.stage` before suffix streaming so peak disk usage stays at
                // `section + max(section, suffix)` on CoW volumes.
                drop(section_stage_file);
                let _ = fs::remove_file(&section_stage_path);

                let new_suffix_start = aligned_start + new_section_len;
                let mut suffix_copied = 0u64;
                if suffix_len > 0 {
                    output_file
                        .seek(SeekFrom::Start(new_suffix_start))
                        .map_err(|error| storage_error(parent_dir, error))?;
                    let Some(streamed) = copy_file_range_streaming(
                        &snapshot_file,
                        aligned_end,
                        suffix_len,
                        &mut output_file,
                        &shared.cancel_requested,
                        Some(&shared.bytes_written),
                        parent_dir,
                    )?
                    else {
                        return Ok(SaveWorkerOutcome::Cancelled);
                    };
                    suffix_copied = streamed;
                }
                drop(snapshot_file);
                let _ = fs::remove_file(&snapshot_stage_path);
                (
                    output_file,
                    SpliceStrategy::CowSnapshotTruncate,
                    sec_written.saturating_add(suffix_copied),
                )
            } else {
                // Unexpectedly lost CoW on second clone; clean up and fall back to full stream.
                drop(output_file);
                let _ = fs::remove_file(&output_stage_path);
                let streaming_remaining =
                    aligned_start.saturating_add(new_section_len.max(suffix_len));
                check_storage(parent_dir, streaming_remaining)?;
                let Some((out, total)) = assemble_streaming_splice(
                    &parent_file,
                    section_stage_file,
                    &section_stage_path,
                    &output_stage_path,
                    aligned_start,
                    aligned_end,
                    new_section_len,
                    suffix_len,
                    parent_dir,
                    shared,
                )?
                else {
                    return Ok(SaveWorkerOutcome::Cancelled);
                };
                (out, SpliceStrategy::StreamingRewrite, total)
            }
        }
    } else {
        let Some((out, total)) = assemble_streaming_splice(
            &parent_file,
            section_stage_file,
            &section_stage_path,
            &output_stage_path,
            aligned_start,
            aligned_end,
            new_section_len,
            suffix_len,
            parent_dir,
            shared,
        )?
        else {
            return Ok(SaveWorkerOutcome::Cancelled);
        };
        (out, SpliceStrategy::StreamingRewrite, total)
    };

    // 5. Restore parent permissions + macOS ACLs, fsync, capture exact published generation guard,
    //    revalidate stamps right before rename, and atomically replace parent.
    if shared.cancel_requested.load(Ordering::Acquire) {
        return Ok(SaveWorkerOutcome::Cancelled);
    }
    output_file
        .set_permissions(parent_permissions)
        .map_err(|error| storage_error(parent_dir, error))?;
    #[cfg(target_os = "macos")]
    set_macos_acl(&output_file, Some(&parent_file))
        .map_err(|error| storage_error(parent_dir, error))?;
    output_file
        .sync_all()
        .map_err(|error| storage_error(parent_dir, error))?;
    let pre_rename_stamp = crate::SourceStamp::from_file(&output_file)?;
    #[cfg(not(unix))]
    drop(output_file);

    if is_working_copy
        && !source_matches_stamp(&section_file, &section_source_path, &section_source_stamp)?
    {
        return Err(QuarryError::SourceChanged);
    }
    if shared.cancel_requested.load(Ordering::Acquire) {
        return Ok(SaveWorkerOutcome::Cancelled);
    }
    let meta = fs::symlink_metadata(&parent_path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            QuarryError::SourceChanged
        } else {
            error.into()
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(QuarryError::InvalidOption(
            "saving through a symbolic link is not supported; use Save As instead",
        ));
    }
    if !source_matches_stamp(&parent_file, &parent_path, &parent_stamp)? {
        return Err(QuarryError::SourceChanged);
    }
    if shared.cancel_requested.load(Ordering::Acquire) {
        return Ok(SaveWorkerOutcome::Cancelled);
    }
    fs::rename(&output_stage_path, &parent_path)
        .map_err(|error| storage_error(parent_dir, error))?;

    // Capture the post-rename stamp (rename updates `ctime` on Linux and `ChangeTime` on Windows)
    // ONLY when `publication_match` confirms the file identity and content metadata still match
    // the exact file we synced before rename.
    #[cfg(unix)]
    let published_stamp = {
        let post = crate::SourceStamp::from_file(&output_file).ok();
        drop(output_file);
        match post {
            Some(post) if pre_rename_stamp.publication_match(&post) != Some(false) => post,
            _ => pre_rename_stamp,
        }
    };
    #[cfg(not(unix))]
    let published_stamp = {
        let post = crate::source_stamp::open_regular_file_nofollow(&parent_path)
            .and_then(|file| crate::SourceStamp::from_file(&file))
            .ok();
        match post {
            Some(post) if pre_rename_stamp.publication_match(&post) != Some(false) => post,
            _ => pre_rename_stamp,
        }
    };
    let published_guard = crate::SourceGuard::from_stamp(published_stamp);

    let new_snapped = ByteWindow::new(aligned_start, new_section_len).ok_or(
        QuarryError::InvalidOption("spliced section bounds overflow"),
    )?;
    let new_bounds = SectionBounds {
        requested: new_snapped,
        snapped: new_snapped,
        trust: BoundaryTrust::Verified,
        proof: parent_bounds.proof,
    };
    let splice_summary = SpliceSaveSummary {
        destination: parent_path.clone(),
        new_bounds,
        new_file_size,
        rows_written,
        section_bytes_written: new_section_len,
        total_bytes_written,
        strategy,
        published_guard,
    };
    Ok(SaveWorkerOutcome::Complete {
        summary: SaveAsSummary {
            destination: parent_path,
            bytes_written: total_bytes_written,
            splice: Some(Box::new(splice_summary)),
        },
        replacements: 0,
        output_index: None,
        effective_workers: 1,
    })
}

#[allow(clippy::too_many_arguments)]
fn assemble_streaming_splice(
    parent_file: &File,
    section_stage_file: File,
    section_stage_path: &Path,
    output_stage_path: &Path,
    aligned_start: u64,
    aligned_end: u64,
    new_section_len: u64,
    suffix_len: u64,
    parent_dir: &Path,
    shared: &SharedState,
) -> Result<Option<(File, u64)>, QuarryError> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut output_file = opts
        .open(output_stage_path)
        .map_err(|error| storage_error(parent_dir, error))?;

    let Some(prefix_copied) = copy_file_range_streaming(
        parent_file,
        0,
        aligned_start,
        &mut output_file,
        &shared.cancel_requested,
        Some(&shared.bytes_written),
        parent_dir,
    )?
    else {
        return Ok(None);
    };
    let Some(section_copied) = copy_file_range_streaming(
        &section_stage_file,
        0,
        new_section_len,
        &mut output_file,
        &shared.cancel_requested,
        None,
        parent_dir,
    )?
    else {
        return Ok(None);
    };
    // Close AND unlink `section.stage` before suffix streaming so its disk blocks are reclaimed.
    drop(section_stage_file);
    let _ = fs::remove_file(section_stage_path);
    let Some(suffix_copied) = copy_file_range_streaming(
        parent_file,
        aligned_end,
        suffix_len,
        &mut output_file,
        &shared.cancel_requested,
        Some(&shared.bytes_written),
        parent_dir,
    )?
    else {
        return Ok(None);
    };
    Ok(Some((
        output_file,
        prefix_copied
            .saturating_add(section_copied)
            .saturating_add(suffix_copied),
    )))
}

/// Start a section-scoped private working-copy rewrite (Insert Rows, Delete Rows, or Replace All).
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_section_working_copy_op(
    source_path: PathBuf,
    source_stamp: crate::SourceStamp,
    parent_bounds: SectionBounds,
    source_had_bom: bool,
    read_base: u64,
    read_end: u64,
    delimiter: u8,
    has_header: bool,
    header_renames: BTreeMap<usize, Vec<u8>>,
    cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
    op: SectionWorkingOp,
    destination: PathBuf,
    config: ExportConfig,
) -> Result<SaveAsJob, QuarryError> {
    if parent_bounds.trust != BoundaryTrust::Verified {
        return Err(QuarryError::InvalidOption(
            "Verify section boundaries before running row edits or Replace All.",
        ));
    }
    if !header_renames.is_empty() {
        return Err(QuarryError::InvalidOption(
            "Header edits are not supported on bounded section documents.",
        ));
    }
    let section_is_interior = parent_bounds.snapped.base > 0;
    let data_start = u64::from(has_header && !section_is_interior);
    if cell_edits
        .first_key_value()
        .is_some_and(|((row, _), _)| *row < data_start)
    {
        return Err(QuarryError::InvalidOption(
            "cell edits must target data rows",
        ));
    }
    for value in cell_edits.values() {
        if value.len() > config.max_record_bytes {
            return Err(QuarryError::RecordTooLarge {
                limit: config.max_record_bytes,
            });
        }
    }
    match &op {
        SectionWorkingOp::InsertRows(ins) => {
            if ins.count == 0 {
                return Err(QuarryError::InvalidOption("insert at least one row"));
            }
            if ins.width == 0 {
                return Err(QuarryError::InvalidOption(
                    "inserted rows require at least one column",
                ));
            }
            if ins.position < data_start {
                return Err(QuarryError::InvalidOption(
                    "rows cannot be inserted above the header row",
                ));
            }
        }
        SectionWorkingOp::DeleteRows(ranges) => {
            if ranges.is_empty() {
                return Err(QuarryError::InvalidOption(
                    "select at least one row to delete",
                ));
            }
            if data_start > 0 && ranges.iter().any(|r| *r.start() < data_start) {
                return Err(QuarryError::InvalidOption(
                    "the header row cannot be deleted",
                ));
            }
        }
        SectionWorkingOp::ReplaceAll(repl) => {
            if repl.needle.is_empty() {
                return Err(QuarryError::InvalidOption("enter text to find"));
            }
            if repl.replacement.len() > config.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
        }
    }

    let source =
        crate::source_stamp::open_regular_file_nofollow(&source_path).map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
            ) {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
    let mut output =
        ExportTarget::new_private_guarded(&source_path, destination, &source, source_stamp)?;
    output.configure(
        config.source_encoding,
        config.output_encoding,
        config.max_record_bytes,
    );
    let publication = output.defer_private_publication();

    let section_len = read_end.saturating_sub(read_base);
    let shared = Arc::new(SharedState::new(section_len.max(1)));
    let worker_state = Arc::clone(&shared);
    let handle = thread::Builder::new()
        .name("quarry-section-rewrite".into())
        .spawn(move || {
            let _completion = WorkerCompletion(&worker_state);
            let result = run_section_working_copy_op(
                source,
                output,
                read_base,
                read_end,
                section_is_interior,
                source_had_bom,
                data_start,
                delimiter,
                &cell_edits,
                op,
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
    Ok(SaveAsJob::from_worker(publication, shared, handle))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn start_section_replace_all(
    source_path: PathBuf,
    source_stamp: crate::SourceStamp,
    parent_bounds: SectionBounds,
    source_had_bom: bool,
    read_base: u64,
    read_end: u64,
    delimiter: u8,
    has_header: bool,
    header_renames: BTreeMap<usize, Vec<u8>>,
    cell_edits: BTreeMap<(u64, usize), Vec<u8>>,
    replacement: LiteralReplacement,
    destination: PathBuf,
    config: ExportConfig,
) -> Result<ReplaceAllJob, QuarryError> {
    let inner = start_section_working_copy_op(
        source_path,
        source_stamp,
        parent_bounds,
        source_had_bom,
        read_base,
        read_end,
        delimiter,
        has_header,
        header_renames,
        cell_edits,
        SectionWorkingOp::ReplaceAll(replacement),
        destination,
        config,
    )?;
    Ok(ReplaceAllJob::from_inner(inner))
}

fn normalize_deleted_ranges(ranges: Vec<RangeInclusive<u64>>) -> Vec<RangeInclusive<u64>> {
    let mut valid: Vec<(u64, u64)> = ranges
        .into_iter()
        .filter(|r| !r.is_empty())
        .map(|r| (*r.start(), *r.end()))
        .collect();
    valid.sort_unstable_by_key(|&(start, _)| start);
    let mut merged: Vec<RangeInclusive<u64>> = Vec::with_capacity(valid.len());
    for (start, end) in valid {
        if let Some(last) = merged.last_mut()
            && start <= last.end().saturating_add(1)
        {
            *last = *last.start()..=(*last.end()).max(end);
        } else {
            merged.push(start..=end);
        }
    }
    merged
}

fn advance_row_deleted(row: u64, deleted: &[RangeInclusive<u64>], cursor: &mut usize) -> bool {
    while let Some(range) = deleted.get(*cursor) {
        if row < *range.start() {
            return false;
        }
        if row <= *range.end() {
            return true;
        }
        *cursor += 1;
    }
    false
}

fn replace_literal_in_field(
    field: &[u8],
    matcher: &ByteMatcher,
    needle_len: usize,
    replacement: &[u8],
    max_bytes: usize,
    limit: usize,
) -> Result<(Vec<u8>, u64), QuarryError> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    let mut count = 0u64;
    while cursor <= field.len() {
        let Some(rel) = matcher.find(&field[cursor..]) else {
            break;
        };
        let hit = cursor + rel;
        let prefix_len = hit - cursor;
        let next_len = out
            .len()
            .saturating_add(prefix_len)
            .saturating_add(replacement.len());
        if next_len > max_bytes {
            return Err(QuarryError::RecordTooLarge { limit });
        }
        if count == 0 {
            out.reserve(field.len().min(max_bytes));
        }
        out.extend_from_slice(&field[cursor..hit]);
        out.extend_from_slice(replacement);
        cursor = hit + needle_len;
        count += 1;
    }
    if count > 0 {
        let tail_len = field.len() - cursor;
        if out.len().saturating_add(tail_len) > max_bytes {
            return Err(QuarryError::RecordTooLarge { limit });
        }
        out.extend_from_slice(&field[cursor..]);
    }
    Ok((out, count))
}

#[allow(clippy::too_many_arguments)]
fn run_section_working_copy_op(
    mut source: File,
    mut output: ExportTarget,
    read_base: u64,
    read_end: u64,
    section_is_interior: bool,
    source_had_bom: bool,
    data_start: u64,
    delimiter: u8,
    cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
    op: SectionWorkingOp,
    config: ExportConfig,
    shared: &SharedState,
) -> Result<SaveWorkerOutcome, QuarryError> {
    if shared.cancel_requested.load(Ordering::Acquire) {
        return Ok(SaveWorkerOutcome::Cancelled);
    }
    let op = match op {
        SectionWorkingOp::DeleteRows(ranges) => {
            SectionWorkingOp::DeleteRows(normalize_deleted_ranges(ranges))
        }
        other => other,
    };
    let section_len = read_end.saturating_sub(read_base);
    let extra_est = match &op {
        SectionWorkingOp::InsertRows(ins) => ins
            .count
            .saturating_mul(ins.width as u64 + 2)
            .saturating_mul(if config.output_encoding.is_utf16() {
                2
            } else {
                1
            }),
        SectionWorkingOp::DeleteRows(_) => 0,
        SectionWorkingOp::ReplaceAll(repl) => {
            if repl.replacement.len() > repl.needle.len() {
                section_len.saturating_mul(2)
            } else {
                0
            }
        }
    };
    let required = crate::estimate_edited_output_bytes_from_values(
        section_len.saturating_add(extra_est),
        0,
        std::iter::empty(),
        cell_edits.iter().map(|(&k, v)| (k, v.as_slice())),
        None,
        None,
    );
    check_storage(output_parent(output.destination()), required)?;
    shared
        .estimated_output_bytes
        .store(required, Ordering::Release);

    // Emit the physical BOM once at byte 0 for BOM-bearing byte-0 sections before any rows or
    // row-0 insertions, and start record scanning past the BOM bytes so row-0 inserts, row-0
    // deletes, and deleting all rows preserve the BOM cleanly.
    let bom = config.source_encoding.bom();
    let scan_start = if !section_is_interior && source_had_bom && !bom.is_empty() {
        let bom_len = bom.len() as u64;
        if read_end.saturating_sub(read_base) >= bom_len {
            let mut probe = [0u8; 3];
            let read = read_at_exact_or_eof(&source, read_base, &mut probe[..bom.len()])?;
            if read == bom.len() && &probe[..bom.len()] == bom {
                output.write_all(UTF8_BOM)?;
                shared
                    .bytes_written
                    .store(output.bytes_written(), Ordering::Release);
                read_base + bom_len
            } else {
                read_base
            }
        } else {
            read_base
        }
    } else {
        read_base
    };
    let wrote_leading_bom = scan_start > read_base;

    let default_ending = detect_encoded_line_ending(
        &source,
        scan_start,
        read_end,
        delimiter,
        config.source_encoding,
    )?;
    let blank_row_utf8: Option<Vec<u8>> = match &op {
        SectionWorkingOp::InsertRows(ins) => {
            let mut row = Vec::with_capacity(ins.width.saturating_sub(1) + default_ending.len());
            for _ in 1..ins.width {
                row.push(delimiter);
            }
            row.extend_from_slice(default_ending);
            Some(row)
        }
        _ => None,
    };
    let matcher = match &op {
        SectionWorkingOp::ReplaceAll(repl) => {
            Some(ByteMatcher::new(&repl.needle, repl.case_sensitivity))
        }
        _ => None,
    };

    if config.source_encoding.is_utf16() {
        source.seek(SeekFrom::Start(scan_start))?;
    }
    let mut scanner =
        crate::encoding::RecordScanner::at_offset(delimiter, scan_start, config.source_encoding)?;
    let mut chunk = vec![0u8; config.source_encoding.chunk_size(config.chunk_bytes)];
    let mut cursor = scan_start;
    let mut local_row = 0u64;
    let mut rows_written = 0u64;
    let mut replacements = 0u64;
    let mut inserted_already = false;
    let mut last_written_had_lf = true;
    let mut record = Vec::new();
    let mut next_cell_edit_row = cell_edits.first_key_value().map(|((row, _), _)| *row);
    let mut delete_cursor = 0usize;

    let write_inserted_rows = |output: &mut ExportTarget,
                               rows_written: &mut u64,
                               last_had_lf: &mut bool|
     -> Result<(), QuarryError> {
        if let SectionWorkingOp::InsertRows(ins) = &op {
            if *rows_written > 0 && !*last_had_lf {
                output.write_all(default_ending)?;
                *last_had_lf = true;
            }
            let row_bytes = blank_row_utf8.as_deref().unwrap();
            for _ in 0..ins.count {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    return Ok(());
                }
                output.write_all(row_bytes)?;
                *rows_written += 1;
            }
            *last_had_lf = true;
            shared
                .bytes_written
                .store(output.bytes_written(), Ordering::Release);
        }
        Ok(())
    };

    let mut process_record = |raw: &[u8],
                              row_idx: u64,
                              output: &mut ExportTarget|
     -> Result<(), QuarryError> {
        if let SectionWorkingOp::InsertRows(ins) = &op
            && !inserted_already
            && row_idx == ins.position
        {
            write_inserted_rows(output, &mut rows_written, &mut last_written_had_lf)?;
            inserted_already = true;
        }
        let has_cell_edit = next_cell_edit_row == Some(row_idx);
        if has_cell_edit {
            next_cell_edit_row = row_idx.checked_add(1).and_then(|next_row| {
                cell_edits
                    .range((next_row, 0)..)
                    .next()
                    .map(|((row, _), _)| *row)
            });
        }
        if let SectionWorkingOp::DeleteRows(deleted) = &op
            && advance_row_deleted(row_idx, deleted, &mut delete_cursor)
        {
            return Ok(());
        }

        let is_first_output_record =
            !section_is_interior && !wrote_leading_bom && rows_written == 0;
        let promoted_literal_bom =
            is_first_output_record && !config.source_encoding.strip_bom(raw).0.is_empty();
        let maybe_replace = matches!(op, SectionWorkingOp::ReplaceAll(_)) && row_idx >= data_start;
        if has_cell_edit || maybe_replace || promoted_literal_bom {
            let decoded = config
                .source_encoding
                .decode(raw, config.max_record_bytes)
                .map_err(|error| crate::record_encoding_error(row_idx.saturating_add(1), error))?;
            let body = decoded.as_ref();
            let ending = if body.ends_with(b"\r\n") {
                b"\r\n".as_slice()
            } else if body.ends_with(b"\n") {
                b"\n".as_slice()
            } else {
                b"".as_slice()
            };
            let mut fields = parse_record(body, delimiter)?
                .into_iter()
                .map(|field| field.into_owned())
                .collect::<Vec<_>>();
            if has_cell_edit {
                let row_edits = cell_edits.range((row_idx, 0)..=(row_idx, usize::MAX));
                if row_edits
                    .clone()
                    .next_back()
                    .is_some_and(|((_, col), _)| *col >= fields.len())
                {
                    return Err(QuarryError::InvalidOption(
                        "cell edit column is out of range",
                    ));
                }
                for ((_, col), value) in row_edits {
                    fields[*col] = value.clone();
                }
            }
            let mut row_replacements = 0u64;
            if let (SectionWorkingOp::ReplaceAll(repl), Some(m)) = (&op, &matcher)
                && row_idx >= data_start
            {
                let mut remaining_budget = config
                    .max_record_bytes
                    .saturating_sub(fields.len().saturating_sub(1))
                    .saturating_sub(ending.len());
                for field in &mut fields {
                    let (replaced, hits) = replace_literal_in_field(
                        field,
                        m,
                        repl.needle.len(),
                        &repl.replacement,
                        remaining_budget,
                        config.max_record_bytes,
                    )?;
                    if hits > 0 {
                        *field = replaced;
                        row_replacements += hits;
                    }
                    if field.len() > remaining_budget {
                        return Err(QuarryError::RecordTooLarge {
                            limit: config.max_record_bytes,
                        });
                    }
                    remaining_budget -= field.len();
                }
                replacements += row_replacements;
            }
            if has_cell_edit || row_replacements > 0 || promoted_literal_bom {
                write_serialized_fields(
                    output,
                    &[],
                    &fields,
                    delimiter,
                    ending,
                    is_first_output_record,
                    config.max_record_bytes,
                )?;
                last_written_had_lf = !ending.is_empty();
            } else {
                output.write_raw_all(raw)?;
                last_written_had_lf = raw_record_has_lf(raw, config.source_encoding);
            }
        } else {
            output.write_raw_all(raw)?;
            last_written_had_lf = raw_record_has_lf(raw, config.source_encoding);
        }
        rows_written += 1;
        shared
            .bytes_written
            .store(output.bytes_written(), Ordering::Release);
        Ok(())
    };

    while cursor < read_end {
        if shared.cancel_requested.load(Ordering::Acquire) {
            return Ok(SaveWorkerOutcome::Cancelled);
        }
        let remaining = read_end - cursor;
        let limit = chunk
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let read = if config.source_encoding.is_utf16() {
            config
                .source_encoding
                .read_chunk(&mut source, &mut chunk[..limit])?
        } else {
            read_at_exact_or_eof(&source, cursor, &mut chunk[..limit])?
        };
        if read == 0 {
            return Err(QuarryError::SourceChanged);
        }
        let mut seg_start = 0usize;
        let mut deferred_error = None;
        let mut cancelled = false;
        let scan_res = scanner.scan_chunk(&chunk[..read], cursor, |abs_end| {
            let local_end = (abs_end - cursor) as usize;
            if deferred_error.is_none() && !cancelled {
                if shared.cancel_requested.load(Ordering::Acquire) {
                    cancelled = true;
                } else {
                    let slice = &chunk[seg_start..local_end];
                    let total_len = record.len().saturating_add(slice.len());
                    if total_len > config.max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: config.max_record_bytes,
                        });
                    } else {
                        let rec: &[u8] = if record.is_empty() {
                            slice
                        } else {
                            record.extend_from_slice(slice);
                            &record
                        };
                        if let Err(err) = process_record(rec, local_row, &mut output) {
                            deferred_error = Some(err);
                        }
                    }
                }
            }
            record.clear();
            local_row += 1;
            seg_start = local_end;
        });
        if cancelled {
            return Ok(SaveWorkerOutcome::Cancelled);
        }
        if let Some(error) = deferred_error {
            return Err(error);
        }
        scan_res?;
        if seg_start < read {
            let tail = &chunk[seg_start..read];
            if record.len().saturating_add(tail.len()) > config.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
            record.extend_from_slice(tail);
        }
        cursor += read as u64;
        shared
            .bytes_scanned
            .store(cursor.saturating_sub(read_base), Ordering::Release);
    }

    let mut finish_err = None;
    let mut finish_cancelled = false;
    scanner.finish(read_end, |_| {
        if shared.cancel_requested.load(Ordering::Acquire) {
            finish_cancelled = true;
            return;
        }
        if !record.is_empty() {
            if let Err(err) = process_record(&record, local_row, &mut output) {
                finish_err = Some(err);
            }
            local_row += 1;
            record.clear();
        }
    })?;
    if finish_cancelled {
        return Ok(SaveWorkerOutcome::Cancelled);
    }
    if let Some(error) = finish_err {
        return Err(error);
    }

    if let SectionWorkingOp::InsertRows(ins) = &op {
        if ins.position > local_row {
            return Err(QuarryError::InvalidOption(
                "row insertion position is out of range",
            ));
        }
        if !inserted_already {
            write_inserted_rows(&mut output, &mut rows_written, &mut last_written_had_lf)?;
        }
    }
    if let Some(((last_edit_row, _), _)) = cell_edits.last_key_value()
        && *last_edit_row >= local_row
    {
        return Err(QuarryError::InvalidOption("cell edit row is out of range"));
    }
    if matches!(op, SectionWorkingOp::ReplaceAll(_)) && replacements == 0 {
        let _ = output.discard();
        return Ok(SaveWorkerOutcome::NoMatch);
    }

    let published_bytes = output.bytes_written();
    match output.publish(rows_written, published_bytes, &shared.cancel_requested)? {
        crate::FilterExportOutcome::Complete(summary) => Ok(SaveWorkerOutcome::Complete {
            summary: SaveAsSummary {
                destination: summary.destination,
                bytes_written: summary.bytes_written,
                splice: None,
            },
            replacements,
            output_index: None,
            effective_workers: 1,
        }),
        crate::FilterExportOutcome::Cancelled => Ok(SaveWorkerOutcome::Cancelled),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tempfile::TempDir;

    #[cfg(target_os = "macos")]
    use crate::SpliceStrategy;
    use crate::{
        BoundaryTrust, ByteWindow, CaseSensitivity, Encoding, FilterOperator, FilterPredicate,
        FilterQuery, HeaderMode, IndexConfig, LiteralReplacement, OpenOptions, QuarryError,
        ReplaceAllOptions, ReplaceAllOutcome, RowInsertion, SaveAsOutcome, SearchOutcome,
        SearchPosition, SectionOpenOptions, Session,
    };

    fn write_fixture(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        let mut f = File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        path
    }

    fn utf16le_bytes(text: &str, with_bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if with_bom {
            out.extend_from_slice(&[0xFF, 0xFE]);
        }
        for u in text.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    }

    fn utf16be_bytes(text: &str, with_bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if with_bom {
            out.extend_from_slice(&[0xFE, 0xFF]);
        }
        for u in text.encode_utf16() {
            out.extend_from_slice(&u.to_be_bytes());
        }
        out
    }

    fn open_verified_section(
        path: &PathBuf,
        window: ByteWindow,
        encoding: Option<Encoding>,
        header_mode: HeaderMode,
    ) -> Session {
        let parent = Session::open_with_encoding(
            path,
            OpenOptions {
                delimiter: Some(b','),
                header_mode,
                ..OpenOptions::default()
            },
            encoding,
        )
        .unwrap();
        let parent_index = parent
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        let sec = Session::open_section(
            path,
            window,
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode,
                    ..OpenOptions::default()
                },
                encoding,
                parent_index: Some(parent_index),
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(sec.section_bounds().unwrap().trust, BoundaryTrust::Verified);
        sec
    }

    #[test]
    fn unedited_verified_section_splice_is_byte_identical() {
        let dir = TempDir::new().unwrap();
        let original =
            b"id,name,note\r\n1,alice,\"line1\r\nline2\"\r\n2,bob,plain\r\n3,cara,end\r\n";
        let path = write_fixture(&dir, "data.csv", original);
        // Interior section covering rows 1 and 2
        let start = b"id,name,note\r\n".len() as u64;
        let len = b"1,alice,\"line1\r\nline2\"\r\n2,bob,plain\r\n".len() as u64;
        let sec = open_verified_section(
            &path,
            ByteWindow::new(start, len).unwrap(),
            None,
            HeaderMode::FirstRow,
        );

        let outcome = sec
            .start_save_with_edits(BTreeMap::new(), BTreeMap::new())
            .unwrap()
            .wait()
            .unwrap();
        let SaveAsOutcome::Complete(summary) = outcome else {
            panic!("expected complete save");
        };
        let splice = summary.splice.expect("splice summary");
        assert_eq!(splice.new_file_size, original.len() as u64);
        assert_eq!(splice.new_bounds.snapped.base, start);
        assert_eq!(splice.new_bounds.snapped.len, len);
        assert_eq!(splice.new_bounds.trust, BoundaryTrust::Verified);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn same_length_shrinking_and_growing_multiline_quoted_splices_preserve_prefix_and_suffix() {
        let dir = TempDir::new().unwrap();
        let prefix = b"id,val\n0,zero\n";
        let middle = b"1,aaaa\n2,bbbb\n";
        let suffix = b"3,tail\n4,last\n";
        let mut initial = Vec::new();
        initial.extend_from_slice(prefix);
        initial.extend_from_slice(middle);
        initial.extend_from_slice(suffix);
        let path = write_fixture(&dir, "splice.csv", &initial);

        let start = prefix.len() as u64;
        let len = middle.len() as u64;

        // 1. Same-length edit
        let sec = open_verified_section(
            &path,
            ByteWindow::new(start, len).unwrap(),
            None,
            HeaderMode::FirstRow,
        );
        let mut edits = BTreeMap::new();
        edits.insert((0, 1), b"ZZZZ".to_vec());
        let SaveAsOutcome::Complete(s1) = sec
            .start_save_with_edits(BTreeMap::new(), edits)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected complete");
        };
        let sp1 = s1.splice.unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(sp1.strategy, SpliceStrategy::CowInPlace);
        assert_eq!(sp1.new_bounds.snapped.len, len);
        let bytes1 = fs::read(&path).unwrap();
        assert_eq!(&bytes1[..start as usize], prefix);
        assert_eq!(
            &bytes1[start as usize..sp1.new_bounds.snapped_end() as usize],
            b"1,ZZZZ\n2,bbbb\n"
        );
        assert_eq!(&bytes1[sp1.new_bounds.snapped_end() as usize..], suffix);

        // 2. Growing multiline quoted edit
        let sec2 = open_verified_section(&path, sp1.new_bounds.snapped, None, HeaderMode::FirstRow);
        let mut edits2 = BTreeMap::new();
        edits2.insert((1, 1), b"multi\n\"quoted\",value".to_vec());
        let SaveAsOutcome::Complete(s2) = sec2
            .start_save_with_edits(BTreeMap::new(), edits2)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected complete");
        };
        let sp2 = s2.splice.unwrap();
        assert!(sp2.new_bounds.snapped.len > len);
        let bytes2 = fs::read(&path).unwrap();
        assert_eq!(&bytes2[..start as usize], prefix);
        assert_eq!(
            &bytes2[start as usize..sp2.new_bounds.snapped_end() as usize],
            b"1,ZZZZ\n2,\"multi\n\"\"quoted\"\",value\"\n"
        );
        assert_eq!(&bytes2[sp2.new_bounds.snapped_end() as usize..], suffix);

        // 3. Shrinking edit
        let sec3 = open_verified_section(&path, sp2.new_bounds.snapped, None, HeaderMode::FirstRow);
        let mut edits3 = BTreeMap::new();
        edits3.insert((1, 1), b"x".to_vec());
        let SaveAsOutcome::Complete(s3) = sec3
            .start_save_with_edits(BTreeMap::new(), edits3)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected complete");
        };
        let sp3 = s3.splice.unwrap();
        assert!(sp3.new_bounds.snapped.len < sp2.new_bounds.snapped.len);
        let bytes3 = fs::read(&path).unwrap();
        assert_eq!(&bytes3[..start as usize], prefix);
        assert_eq!(
            &bytes3[start as usize..sp3.new_bounds.snapped_end() as usize],
            b"1,ZZZZ\n2,x\n"
        );
        assert_eq!(&bytes3[sp3.new_bounds.snapped_end() as usize..], suffix);
    }

    #[test]
    fn insert_delete_and_replace_all_working_copy_splice_round_trip() {
        let dir = TempDir::new().unwrap();
        let prefix = b"id,tag\n0,old\n";
        let middle = b"1,old\n2,keep\n3,old\n";
        let suffix = b"4,old\n";
        let mut initial = Vec::new();
        initial.extend_from_slice(prefix);
        initial.extend_from_slice(middle);
        initial.extend_from_slice(suffix);
        let path = write_fixture(&dir, "ops.csv", &initial);
        let start = prefix.len() as u64;
        let len = middle.len() as u64;

        let orig = open_verified_section(
            &path,
            ByteWindow::new(start, len).unwrap(),
            None,
            HeaderMode::FirstRow,
        );

        // Insert 2 blank rows before local row 1
        let wc1_path = dir.path().join("wc1.csv");
        let SaveAsOutcome::Complete(_) = orig
            .start_create_working_copy_inserting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                RowInsertion {
                    position: 1,
                    count: 2,
                    width: 2,
                },
                &wc1_path,
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected working copy");
        };
        let wc1 =
            Session::open_section_working_copy(&wc1_path, &orig, OpenOptions::default()).unwrap();
        assert!(wc1.section_backed_by_working_copy());
        assert!(wc1.section_is_interior());
        assert_eq!(wc1.first_rows.len(), 5);

        // Replace All "old" -> "NEW" inside section only (must not touch prefix or suffix!)
        let wc2_path = dir.path().join("wc2.csv");
        let ReplaceAllOutcome::Complete(rep) = wc1
            .start_create_replaced_working_copy_with_options(
                BTreeMap::new(),
                BTreeMap::new(),
                LiteralReplacement {
                    needle: b"old".to_vec(),
                    replacement: b"NEW".to_vec(),
                    case_sensitivity: CaseSensitivity::Sensitive,
                },
                &wc2_path,
                ReplaceAllOptions::default(),
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected replace complete");
        };
        assert_eq!(rep.replacements, 2);
        let wc2 =
            Session::open_section_working_copy(&wc2_path, &orig, OpenOptions::default()).unwrap();

        // Delete row 3 (the original "2,keep")
        let wc3_path = dir.path().join("wc3.csv");
        let SaveAsOutcome::Complete(_) = wc2
            .start_create_working_copy_deleting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                std::iter::once(3..=3).collect(),
                &wc3_path,
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected delete complete");
        };
        let wc3 =
            Session::open_section_working_copy(&wc3_path, &orig, OpenOptions::default()).unwrap();
        // Splice working copy back to original parent
        let SaveAsOutcome::Complete(saved) = wc3
            .start_save_to_original(BTreeMap::new(), BTreeMap::new(), &orig)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected splice save");
        };
        let sp = saved.splice.unwrap();
        assert_eq!(sp.rows_written, 4);
        assert_eq!(
            fs::read(&path).unwrap(),
            b"id,tag\n0,old\n1,NEW\n,\n,\n3,NEW\n4,old\n"
        );
    }

    #[test]
    fn delete_all_rows_splices_to_zero_byte_verified_section() {
        let dir = TempDir::new().unwrap();
        let prefix = b"id,v\n0,p\n";
        let middle = b"1,a\n2,b\n";
        let suffix = b"3,s\n";
        let mut initial = Vec::new();
        initial.extend_from_slice(prefix);
        initial.extend_from_slice(middle);
        initial.extend_from_slice(suffix);
        let path = write_fixture(&dir, "empty_sec.csv", &initial);
        let start = prefix.len() as u64;
        let len = middle.len() as u64;
        let orig = open_verified_section(
            &path,
            ByteWindow::new(start, len).unwrap(),
            None,
            HeaderMode::FirstRow,
        );

        let wc_path = dir.path().join("wc_empty.csv");
        orig.start_create_working_copy_deleting_rows(
            BTreeMap::new(),
            BTreeMap::new(),
            std::iter::once(0..=1).collect(),
            &wc_path,
        )
        .unwrap()
        .wait()
        .unwrap();
        let wc =
            Session::open_section_working_copy(&wc_path, &orig, OpenOptions::default()).unwrap();
        assert!(wc.first_rows.is_empty());

        let SaveAsOutcome::Complete(saved) = wc
            .start_save_to_original(BTreeMap::new(), BTreeMap::new(), &orig)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected splice save");
        };
        let sp = saved.splice.unwrap();
        assert_eq!(sp.rows_written, 0);
        assert_eq!(sp.new_bounds.snapped.base, start);
        assert_eq!(sp.new_bounds.snapped.len, 0);
        assert_eq!(fs::read(&path).unwrap(), b"id,v\n0,p\n3,s\n");

        // Reopen preserved 0-byte verified section cleanly!
        let guard = Session::open(&path, OpenOptions::default())
            .unwrap()
            .source_guard();
        let reopened = Session::open_section_preserved(
            &path,
            sp.new_bounds,
            &guard,
            b',',
            Encoding::Utf8,
            SectionOpenOptions::default(),
        )
        .unwrap();
        assert_eq!(
            reopened.section_bounds().unwrap().trust,
            BoundaryTrust::Verified
        );
        assert!(reopened.first_rows.is_empty());
    }

    #[test]
    fn interior_working_copy_preserves_literal_leading_ufeff_across_read_filter_search_and_save() {
        let dir = TempDir::new().unwrap();
        let prefix = b"id,val\n0,hdr\n";
        let middle = b"\xEF\xBB\xBF1,alpha\n2,beta\n";
        let suffix = b"3,gamma\n";
        let mut initial = Vec::new();
        initial.extend_from_slice(prefix);
        initial.extend_from_slice(middle);
        initial.extend_from_slice(suffix);
        let path = write_fixture(&dir, "ufeff.csv", &initial);
        let orig = open_verified_section(
            &path,
            ByteWindow::new(prefix.len() as u64, middle.len() as u64).unwrap(),
            None,
            HeaderMode::FirstRow,
        );
        // Replace "beta" -> "BETA" to materialize a section working copy starting at offset 0.
        let wc_path = dir.path().join("wc_ufeff.csv");
        orig.start_create_replaced_working_copy_with_options(
            BTreeMap::new(),
            BTreeMap::new(),
            LiteralReplacement {
                needle: b"beta".to_vec(),
                replacement: b"BETA".to_vec(),
                case_sensitivity: CaseSensitivity::Sensitive,
            },
            &wc_path,
            ReplaceAllOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap();
        let wc =
            Session::open_section_working_copy(&wc_path, &orig, OpenOptions::default()).unwrap();
        assert_eq!(wc.first_rows[0].fields[0], b"\xEF\xBB\xBF1");

        // Indexing + read_rows preserves leading U+FEFF
        let idx = wc
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        let rows = wc.read_rows(&idx, 0, 2).unwrap();
        assert_eq!(rows[0].fields[0], b"\xEF\xBB\xBF1");

        // Search preserves leading U+FEFF
        let SearchOutcome::Match(m) = wc
            .start_search(
                &idx,
                b"\xEF\xBB\xBF1".to_vec(),
                SearchPosition { row: 0, column: 0 },
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected U+FEFF search hit");
        };
        assert_eq!((m.row, m.column), (0, 0));

        // Filter preserves leading U+FEFF
        let fidx = wc
            .start_filter(FilterQuery {
                predicates: vec![FilterPredicate {
                    column: 0,
                    operator: FilterOperator::Equals,
                    value: b"\xEF\xBB\xBF1".to_vec(),
                    upper_bound: None,
                }],
                case_sensitivity: CaseSensitivity::Sensitive,
            })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(fidx.matches_found(), 1);
        let frows = wc.read_filtered_rows(&fidx, 0, 1).unwrap();
        assert_eq!(frows[0].fields[0], b"\xEF\xBB\xBF1");

        // Save Section As New still prepends the captured byte-0 header row!
        let export_path = dir.path().join("export.csv");
        wc.start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), &export_path)
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            fs::read(&export_path).unwrap(),
            b"id,val\n\xEF\xBB\xBF1,alpha\n2,BETA\n"
        );
    }

    #[test]
    fn utf16le_and_utf16be_sections_splice_cleanly_without_corrupting_bom_or_encoding() {
        let dir = TempDir::new().unwrap();
        for (enc, with_bom, name) in [
            (Encoding::Utf16Le, true, "le_bom.csv"),
            (Encoding::Utf16Le, false, "le_nobom.csv"),
            (Encoding::Utf16Be, true, "be_bom.csv"),
            (Encoding::Utf16Be, false, "be_nobom.csv"),
        ] {
            let make = |s: &str, bom: bool| match enc {
                Encoding::Utf16Le => utf16le_bytes(s, bom),
                Encoding::Utf16Be => utf16be_bytes(s, bom),
                _ => unreachable!(),
            };
            let prefix = make("id,name\n0,head\n", with_bom);
            let middle = make("1,alice\n2,bob\n", false);
            let suffix = make("3,tail\n", false);
            let mut full = Vec::new();
            full.extend_from_slice(&prefix);
            full.extend_from_slice(&middle);
            full.extend_from_slice(&suffix);
            let path = write_fixture(&dir, name, &full);
            let sec = open_verified_section(
                &path,
                ByteWindow::new(prefix.len() as u64, middle.len() as u64).unwrap(),
                Some(enc),
                HeaderMode::FirstRow,
            );
            let mut edits = BTreeMap::new();
            edits.insert((0, 1), b"alicia_longer".to_vec());
            let SaveAsOutcome::Complete(saved) = sec
                .start_save_with_edits(BTreeMap::new(), edits)
                .unwrap()
                .wait()
                .unwrap()
            else {
                panic!("expected save complete for {name}");
            };
            let sp = saved.splice.unwrap();
            let mut expected = Vec::new();
            expected.extend_from_slice(&prefix);
            expected.extend_from_slice(&make("1,alicia_longer\n2,bob\n", false));
            expected.extend_from_slice(&suffix);
            assert_eq!(fs::read(&path).unwrap(), expected, "mismatch for {name}");
            assert_eq!(sp.new_file_size, expected.len() as u64);
        }
    }

    #[test]
    fn speculative_header_rename_source_changed_and_symlink_are_rejected() {
        let dir = TempDir::new().unwrap();
        // Build a file > 16 MiB so a far-away section opens Speculative!
        // Actually even simpler: write 17 MiB of rows.
        let mut big = Vec::with_capacity(17 * 1024 * 1024);
        big.extend_from_slice(b"id,val\n");
        let row = b"000001,abcdefghij0123456789abcdefghij\n";
        while big.len() < 17 * 1024 * 1024 {
            big.extend_from_slice(row);
        }
        let big_path = write_fixture(&dir, "big.csv", &big);
        let spec_sec = Session::open_section(
            &big_path,
            ByteWindow::new(16 * 1024 * 1024 + 512 * 1024, 4096).unwrap(),
            SectionOpenOptions::default(),
        )
        .unwrap();
        assert_eq!(
            spec_sec.section_bounds().unwrap().trust,
            BoundaryTrust::Speculative
        );
        assert!(matches!(
            spec_sec.start_save_with_edits(BTreeMap::new(), BTreeMap::new()),
            Err(QuarryError::InvalidOption(_))
        ));
        assert!(matches!(
            spec_sec.start_create_working_copy_inserting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                RowInsertion {
                    position: 0,
                    count: 1,
                    width: 2
                },
                dir.path().join("wc_spec.csv")
            ),
            Err(QuarryError::InvalidOption(_))
        ));

        // Verified section rejects header rename & external modification (SourceChanged)
        let small_path = write_fixture(&dir, "small.csv", b"id,val\n1,a\n2,b\n");
        let sec = open_verified_section(
            &small_path,
            ByteWindow::new(7, 8).unwrap(),
            None,
            HeaderMode::FirstRow,
        );
        let mut renames = BTreeMap::new();
        renames.insert(0, b"ID".to_vec());
        assert!(matches!(
            sec.start_save_with_edits(renames, BTreeMap::new()),
            Err(QuarryError::InvalidOption(_))
        ));
        // Modify file externally -> SourceChanged
        fs::write(&small_path, b"id,val\n1,CHANGED\n2,b\n").unwrap();
        assert!(matches!(
            sec.start_save_with_edits(BTreeMap::new(), BTreeMap::new()),
            Err(QuarryError::SourceChanged)
        ));

        // Symlink rejection
        #[cfg(unix)]
        {
            let target = write_fixture(&dir, "real.csv", b"id,val\n1,a\n2,b\n");
            let link = dir.path().join("link.csv");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            // Open section on target then replace path with symlink before save
            let sec2 = open_verified_section(
                &target,
                ByteWindow::new(7, 8).unwrap(),
                None,
                HeaderMode::FirstRow,
            );
            let backup = dir.path().join("backup.csv");
            fs::rename(&target, &backup).unwrap();
            std::os::unix::fs::symlink(&backup, &target).unwrap();
            assert!(matches!(
                sec2.start_save_with_edits(BTreeMap::new(), BTreeMap::new()),
                Err(QuarryError::InvalidOption(_))
            ));
        }
    }

    #[test]
    fn filtered_section_splice_preserves_hidden_rows_and_preflight_works() {
        let dir = TempDir::new().unwrap();
        let path = write_fixture(
            &dir,
            "filt.csv",
            b"id,keep\n0,head\n1,yes\n2,no\n3,yes\n4,tail\n",
        );
        let start = b"id,keep\n0,head\n".len() as u64;
        let len = b"1,yes\n2,no\n3,yes\n".len() as u64;
        let sec = open_verified_section(
            &path,
            ByteWindow::new(start, len).unwrap(),
            None,
            HeaderMode::FirstRow,
        );

        let preflight = sec
            .estimate_splice_save_storage(None, &BTreeMap::new())
            .unwrap();
        assert!(preflight.available_bytes > 0);
        assert!(preflight.estimated_required_bytes > 0);

        // Filter to keep == "yes", edit visible row 0 ("1,yes" -> "1,YES"), then splice-save.
        let mut edits = BTreeMap::new();
        edits.insert((0, 1), b"YES".to_vec());
        let fidx = sec
            .start_filter_with_edits(
                FilterQuery {
                    predicates: vec![FilterPredicate {
                        column: 1,
                        operator: FilterOperator::Equals,
                        value: b"yes".to_vec(),
                        upper_bound: None,
                    }],
                    case_sensitivity: CaseSensitivity::Insensitive,
                },
                Arc::new(
                    edits
                        .iter()
                        .map(|(&k, v)| (k, Arc::new(v.clone())))
                        .collect(),
                ),
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(fidx.matches_found(), 2);

        sec.start_save_with_edits(BTreeMap::new(), edits)
            .unwrap()
            .wait()
            .unwrap();
        // Hidden row "2,no" is preserved intact!
        assert_eq!(
            fs::read(&path).unwrap(),
            b"id,keep\n0,head\n1,YES\n2,no\n3,yes\n4,tail\n"
        );
    }

    #[test]
    fn splice_staging_leaves_zero_orphan_entries_in_parent_dir() {
        let dir = TempDir::new().unwrap();
        let path = write_fixture(&dir, "clean.csv", b"id,v\n0,a\n1,b\n2,c\n");
        let sec = open_verified_section(
            &path,
            ByteWindow::new(9, 4).unwrap(),
            None,
            HeaderMode::FirstRow,
        );
        let mut edits = BTreeMap::new();
        edits.insert((0, 1), b"bbbbbbbb".to_vec());
        sec.start_save_with_edits(BTreeMap::new(), edits)
            .unwrap()
            .wait()
            .unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("clean.csv")]);
    }

    #[test]
    fn large_synthetic_file_cow_splice_preserves_prefix_and_suffix_byte_for_byte() {
        let dir = TempDir::new().unwrap();
        let mut prefix = Vec::from(&b"id,station,value,note\n"[..]);
        for i in 0..40_000u64 {
            prefix.extend_from_slice(
                format!(
                    "{i:06},STN_{:03},{:.2},prefix_payload_text\n",
                    i % 100,
                    i as f64 * 0.25
                )
                .as_bytes(),
            );
        }
        let middle =
            b"040000,STN_000,10000.00,target_row_alpha\n040001,STN_001,10000.25,target_row_beta\n";
        let mut suffix = Vec::new();
        for i in 40_002..80_000u64 {
            suffix.extend_from_slice(
                format!(
                    "{i:06},STN_{:03},{:.2},suffix_payload_text\n",
                    i % 100,
                    i as f64 * 0.25
                )
                .as_bytes(),
            );
        }
        let mut full = Vec::with_capacity(prefix.len() + middle.len() + suffix.len());
        full.extend_from_slice(&prefix);
        full.extend_from_slice(middle);
        full.extend_from_slice(&suffix);
        let path = write_fixture(&dir, "large_oracle.csv", &full);

        let sec_start = prefix.len() as u64;
        let sec_len = middle.len() as u64;

        // 1. Same-length splice (APFS CowInPlace on macOS)
        let sec = open_verified_section(
            &path,
            ByteWindow::new(sec_start, sec_len).unwrap(),
            None,
            HeaderMode::FirstRow,
        );
        let mut edits = BTreeMap::new();
        edits.insert((0, 3), b"TARGET_ROW_ALPHA".to_vec());
        let SaveAsOutcome::Complete(s1) = sec
            .start_save_with_edits(BTreeMap::new(), edits)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected complete same-length splice");
        };
        let sp1 = s1.splice.unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(sp1.strategy, SpliceStrategy::CowInPlace);
        let after1 = fs::read(&path).unwrap();
        assert_eq!(&after1[..prefix.len()], prefix.as_slice());
        assert_eq!(&after1[prefix.len() + middle.len()..], suffix.as_slice());

        // 2. Growing length-changing splice (APFS CowSnapshotTruncate on macOS)
        let sec2 = open_verified_section(&path, sp1.new_bounds.snapped, None, HeaderMode::FirstRow);
        let mut edits2 = BTreeMap::new();
        edits2.insert(
            (1, 3),
            b"expanded_multiline\n\"quoted\",payload_for_oracle".to_vec(),
        );
        let SaveAsOutcome::Complete(s2) = sec2
            .start_save_with_edits(BTreeMap::new(), edits2)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected complete growing splice");
        };
        let sp2 = s2.splice.unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(sp2.strategy, SpliceStrategy::CowSnapshotTruncate);
        let after2 = fs::read(&path).unwrap();
        assert_eq!(&after2[..prefix.len()], prefix.as_slice());
        assert_eq!(
            &after2[sp2.new_bounds.snapped_end() as usize..],
            suffix.as_slice()
        );
        let expected_mid = b"040000,STN_000,10000.00,TARGET_ROW_ALPHA\n040001,STN_001,10000.25,\"expanded_multiline\n\"\"quoted\"\",payload_for_oracle\"\n";
        assert_eq!(
            &after2[prefix.len()..sp2.new_bounds.snapped_end() as usize],
            expected_mid.as_slice()
        );
    }

    #[test]
    fn bom_bearing_headerless_byte_zero_section_preserves_bom_across_row_insert_delete_and_delete_all()
     {
        let dir = TempDir::new().unwrap();
        let initial = b"\xEF\xBB\xBFalpha,1\nbeta,2\ngamma,3\n";
        let path = write_fixture(&dir, "bom_headerless.csv", initial);
        // Byte-0 section covering only the first two rows ("alpha,1\nbeta,2\n"), leaving "gamma,3\n" as suffix.
        let sec_len = b"\xEF\xBB\xBFalpha,1\nbeta,2\n".len() as u64;
        let sec = open_verified_section(
            &path,
            ByteWindow::new(0, sec_len).unwrap(),
            None,
            HeaderMode::NoHeader,
        );

        // 1. Insert 1 blank row at row 0 -> BOM must stay at byte 0 before the blank row.
        let wc_ins = dir.path().join("wc_ins.csv");
        let SaveAsOutcome::Complete(_) = sec
            .start_create_working_copy_inserting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                RowInsertion {
                    position: 0,
                    count: 1,
                    width: 2,
                },
                &wc_ins,
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected working copy insert");
        };
        assert_eq!(
            fs::read(&wc_ins).unwrap(),
            b"\xEF\xBB\xBF,\nalpha,1\nbeta,2\n"
        );

        // 2. Delete row 0 -> BOM must stay at byte 0 before "beta,2\n".
        let wc_del0 = dir.path().join("wc_del0.csv");
        let SaveAsOutcome::Complete(_) = sec
            .start_create_working_copy_deleting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                std::iter::once(0..=0).collect(),
                &wc_del0,
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected working copy delete row 0");
        };
        assert_eq!(fs::read(&wc_del0).unwrap(), b"\xEF\xBB\xBFbeta,2\n");

        // 3. Delete ALL rows in the section -> BOM must still remain at byte 0, and splicing back
        //    into the parent file with trailing suffix must NOT insert a phantom empty row!
        let wc_del_all = dir.path().join("wc_del_all.csv");
        let SaveAsOutcome::Complete(_) = sec
            .start_create_working_copy_deleting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                std::iter::once(0..=1).collect(),
                &wc_del_all,
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected working copy delete all");
        };
        assert_eq!(fs::read(&wc_del_all).unwrap(), b"\xEF\xBB\xBF");

        let wc_session =
            Session::open_section_working_copy(&wc_del_all, &sec, OpenOptions::default()).unwrap();
        assert!(
            wc_session.first_rows.is_empty(),
            "BOM-only section working copy must bootstrap with 0 rows"
        );
        let wc_idx = wc_session
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            wc_idx.indexed_rows(),
            0,
            "BOM-only section working copy must index 0 rows"
        );
        let SaveAsOutcome::Complete(saved) = wc_session
            .start_save_to_original(BTreeMap::new(), BTreeMap::new(), &sec)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected splice save complete");
        };
        let sp = saved.splice.unwrap();
        assert_eq!(sp.rows_written, 0);
        assert_eq!(sp.section_bytes_written, 3);
        assert_eq!(fs::read(&path).unwrap(), b"\xEF\xBB\xBFgamma,3\n");

        // Reopening the spliced BOM-only section `[0, 3)` must also report 0 bootstrap & indexed rows.
        let reopened = Session::open_section_preserved(
            &path,
            sp.new_bounds,
            &sp.published_guard,
            b',',
            Encoding::Utf8,
            SectionOpenOptions {
                open: OpenOptions {
                    delimiter: Some(b','),
                    header_mode: HeaderMode::NoHeader,
                    ..OpenOptions::default()
                },
                ..SectionOpenOptions::default()
            },
        )
        .unwrap();
        assert!(
            reopened.first_rows.is_empty(),
            "reopened BOM-only spliced section must bootstrap with 0 rows"
        );
        let reopened_idx = reopened
            .start_indexing(IndexConfig::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            reopened_idx.indexed_rows(),
            0,
            "reopened BOM-only spliced section must index 0 rows"
        );
    }

    #[test]
    fn cleared_single_column_unterminated_final_row_persists_as_empty_quoted_record() {
        let dir = TempDir::new().unwrap();
        let path = write_fixture(&dir, "one_col_no_lf.csv", b"col\nfirst\nx");
        let sec = open_verified_section(
            &path,
            ByteWindow::new(0, 11).unwrap(),
            None,
            HeaderMode::FirstRow,
        );
        let mut edits = BTreeMap::new();
        edits.insert((2, 0), Vec::new());
        let SaveAsOutcome::Complete(_) = sec
            .start_save_with_edits(BTreeMap::new(), edits)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected complete splice");
        };
        assert_eq!(fs::read(&path).unwrap(), b"col\nfirst\n\"\"");
    }

    #[test]
    fn bomless_byte_zero_section_quotes_literal_feff_on_edit_and_row_delete_promotion() {
        let dir = TempDir::new().unwrap();
        // 1. Direct cell edit introducing literal U+FEFF at byte 0 of a BOM-less file.
        let p1 = write_fixture(&dir, "bomless_edit.csv", b"alpha,1\nbeta,2\n");
        let sec1 = open_verified_section(
            &p1,
            ByteWindow::new(0, 15).unwrap(),
            None,
            HeaderMode::NoHeader,
        );
        let mut edits = BTreeMap::new();
        edits.insert((0, 0), b"\xEF\xBB\xBFalpha".to_vec());
        let SaveAsOutcome::Complete(_) = sec1
            .start_save_with_edits(BTreeMap::new(), edits)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected complete splice");
        };
        assert_eq!(fs::read(&p1).unwrap(), b"\"\xEF\xBB\xBFalpha\",1\nbeta,2\n");
        // Reopening must preserve literal U+FEFF in cell (0, 0), NOT strip it as a BOM.
        let reopened = Session::open(
            &p1,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(reopened.first_rows[0].fields[0], b"\xEF\xBB\xBFalpha");

        // 2. Deleting row 0 promotes row 1 starting with literal unquoted U+FEFF to byte 0.
        let p2 = write_fixture(&dir, "bomless_del.csv", b"drop,0\n\xEF\xBB\xBFkeep,1\n");
        let sec2 = open_verified_section(
            &p2,
            ByteWindow::new(0, fs::metadata(&p2).unwrap().len()).unwrap(),
            None,
            HeaderMode::NoHeader,
        );
        let wc = dir.path().join("wc_promoted.csv");
        let SaveAsOutcome::Complete(_) = sec2
            .start_create_working_copy_deleting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                std::iter::once(0..=0).collect(),
                &wc,
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected delete working copy");
        };
        assert_eq!(fs::read(&wc).unwrap(), b"\"\xEF\xBB\xBFkeep\",1\n");
    }

    #[test]
    fn section_replace_all_rejects_oversized_expansion_before_unbounded_growth() {
        let dir = TempDir::new().unwrap();
        // 200 occurrences of 'a' replaced by 500_000 bytes each would be 100 MiB (> 64 MiB default
        // `DEFAULT_MAX_RECORD_BYTES` limit).
        let mut content = Vec::from(&b"id,text\n1,"[..]);
        content.extend(std::iter::repeat_n(b'a', 200));
        content.push(b'\n');
        let path = write_fixture(&dir, "replace_oversize.csv", &content);
        let sec = open_verified_section(
            &path,
            ByteWindow::new(0, content.len() as u64).unwrap(),
            None,
            HeaderMode::FirstRow,
        );
        let wc = dir.path().join("wc_oversize.csv");
        let err = sec
            .start_create_replaced_working_copy_with_options(
                BTreeMap::new(),
                BTreeMap::new(),
                LiteralReplacement {
                    needle: b"a".to_vec(),
                    replacement: vec![b'X'; 500_000],
                    case_sensitivity: CaseSensitivity::Sensitive,
                },
                &wc,
                ReplaceAllOptions::default(),
            )
            .unwrap()
            .wait()
            .unwrap_err();
        assert!(
            matches!(err, QuarryError::RecordTooLarge { .. }),
            "expected RecordTooLarge, got {err:?}"
        );
    }

    #[test]
    fn mismatched_section_save_to_original_is_rejected() {
        let dir = TempDir::new().unwrap();
        let data = b"id,val\n1,aaa\n2,bbb\n3,ccc\n4,ddd\n";
        let path = write_fixture(&dir, "two_sections.csv", data);
        let sec_a = open_verified_section(
            &path,
            ByteWindow::new(7, 6).unwrap(), // "1,aaa\n"
            None,
            HeaderMode::FirstRow,
        );
        let sec_b = open_verified_section(
            &path,
            ByteWindow::new(13, 6).unwrap(), // "2,bbb\n"
            None,
            HeaderMode::FirstRow,
        );
        let err = match sec_a.start_save_to_original(BTreeMap::new(), BTreeMap::new(), &sec_b) {
            Err(err) => err,
            Ok(_) => panic!("expected mismatched section provenance to be rejected"),
        };
        assert!(
            matches!(err, QuarryError::InvalidOption(_)),
            "expected InvalidOption for mismatched section bounds, got {err:?}"
        );
        assert_eq!(fs::read(&path).unwrap(), data);
    }

    #[cfg(unix)]
    #[test]
    fn readonly_mode_0444_parent_splice_save_succeeds_and_preserves_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let path = write_fixture(&dir, "readonly_parent.csv", b"id,val\n1,aaa\n2,bbb\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();

        let sec = open_verified_section(
            &path,
            ByteWindow::new(7, 6).unwrap(),
            None,
            HeaderMode::FirstRow,
        );
        let mut edits = BTreeMap::new();
        edits.insert((0, 1), b"ZZZ".to_vec());
        let SaveAsOutcome::Complete(summary) = sec
            .start_save_with_edits(BTreeMap::new(), edits)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected complete splice on 0444 parent");
        };
        let sp = summary.splice.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"id,val\n1,ZZZ\n2,bbb\n");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444);
        assert!(sp.published_guard.matches_path(&path).unwrap());

        // Restore write bit for clean TempDir teardown on strict environments.
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
    }

    #[test]
    fn splice_summary_published_guard_matches_published_file_and_rejects_post_save_tamper() {
        let dir = TempDir::new().unwrap();
        let path = write_fixture(&dir, "guard_check.csv", b"id,val\n1,aaa\n2,bbb\n");
        let sec = open_verified_section(
            &path,
            ByteWindow::new(7, 6).unwrap(),
            None,
            HeaderMode::FirstRow,
        );
        let mut edits = BTreeMap::new();
        edits.insert((0, 1), b"AAA".to_vec());
        let SaveAsOutcome::Complete(summary) = sec
            .start_save_with_edits(BTreeMap::new(), edits)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected complete splice");
        };
        let sp = summary.splice.unwrap();
        assert!(sp.published_guard.matches_path(&path).unwrap());

        // External write right after splice invalidates `published_guard`.
        fs::write(&path, b"id,val\n1,EXT\n2,bbb\n3,ccc\n").unwrap();
        assert!(!sp.published_guard.matches_path(&path).unwrap());
    }

    #[test]
    fn deleting_all_rows_in_bomless_byte_zero_section_rejects_suffix_starting_with_literal_feff() {
        let dir = TempDir::new().unwrap();
        let initial = b"1,a\n\xEF\xBB\xBF2,b\n3,c\n";
        let path = write_fixture(&dir, "suffix_feff_promotion.csv", initial);
        let sec = open_verified_section(
            &path,
            ByteWindow::new(0, 4).unwrap(), // "1,a\n"
            None,
            HeaderMode::NoHeader,
        );
        let wc = dir.path().join("wc_empty.csv");
        let SaveAsOutcome::Complete(_) = sec
            .start_create_working_copy_deleting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                std::iter::once(0..=0).collect(),
                &wc,
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected delete working copy");
        };
        let wc_session =
            Session::open_section_working_copy(&wc, &sec, OpenOptions::default()).unwrap();
        let err = wc_session
            .start_save_to_original(BTreeMap::new(), BTreeMap::new(), &sec)
            .unwrap()
            .wait()
            .unwrap_err();
        assert!(
            matches!(err, QuarryError::InvalidOption(_)),
            "expected InvalidOption guarding literal BOM suffix promotion, got {err:?}"
        );
        // Parent file must remain completely untouched.
        assert_eq!(fs::read(&path).unwrap(), initial);
    }

    #[test]
    fn section_delete_rows_merges_and_advances_cursor_over_many_disjoint_ranges() {
        let dir = TempDir::new().unwrap();
        let mut content = Vec::from(&b"id,val\n"[..]);
        for i in 0..2000u64 {
            content.extend_from_slice(format!("{i},v{i}\n").as_bytes());
        }
        let path = write_fixture(&dir, "many_disjoint_deletes.csv", &content);
        let sec = open_verified_section(
            &path,
            ByteWindow::new(7, (content.len() - 7) as u64).unwrap(),
            None,
            HeaderMode::FirstRow,
        );
        // Delete all even rows using unsorted & overlapping single-row/short ranges.
        let mut ranges: Vec<std::ops::RangeInclusive<u64>> =
            (0..1000u64).rev().map(|k| (k * 2)..=(k * 2)).collect();
        ranges.push(10..=12);
        let wc = dir.path().join("wc_disjoint.csv");
        let SaveAsOutcome::Complete(_) = sec
            .start_create_working_copy_deleting_rows(BTreeMap::new(), BTreeMap::new(), ranges, &wc)
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected working copy delete");
        };
        let out = String::from_utf8(fs::read(&wc).unwrap()).unwrap();
        let kept_ids: Vec<u64> = out
            .lines()
            .map(|line| line.split(',').next().unwrap().parse::<u64>().unwrap())
            .collect();
        let expected: Vec<u64> = (0..2000u64)
            .filter(|&r| r % 2 == 1 && !(10..=12).contains(&r))
            .collect();
        assert_eq!(kept_ids, expected);
    }

    #[test]
    fn section_row_insert_detects_crlf_at_record_boundary_ignoring_quoted_lf() {
        let dir = TempDir::new().unwrap();
        let content = b"\"first\nline\",x\r\nnext,y\r\n";
        let path = write_fixture(&dir, "quoted_lf_crlf.csv", content);
        let sec = open_verified_section(
            &path,
            ByteWindow::new(0, content.len() as u64).unwrap(),
            None,
            HeaderMode::NoHeader,
        );
        let wc = dir.path().join("wc_crlf.csv");
        let SaveAsOutcome::Complete(_) = sec
            .start_create_working_copy_inserting_rows(
                BTreeMap::new(),
                BTreeMap::new(),
                RowInsertion {
                    position: 1,
                    count: 1,
                    width: 2,
                },
                &wc,
            )
            .unwrap()
            .wait()
            .unwrap()
        else {
            panic!("expected row insert complete");
        };
        assert_eq!(
            fs::read(&wc).unwrap(),
            b"\"first\nline\",x\r\n,\r\nnext,y\r\n"
        );
    }
}
