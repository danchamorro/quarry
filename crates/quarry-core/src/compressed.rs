//! Bounded, read-only access to gzip-compressed delimited files.
//!
//! A compressed source is never parsed as delimited text. A bounded bootstrap
//! decompresses only the bytes needed for the first rows. A background job then
//! streams the whole source once into an owner-only private file while building
//! the structural index, so no second indexing pass is needed. Every gzip member
//! is verified (header, deflate stream, CRC-32 and ISIZE) and data after the last
//! member is rejected instead of being silently ignored.

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flate2::{Crc, Decompress, FlushDecompress, Status};

use crate::encoding::{EncodedScanner, RecordScanner};
use crate::export::{open_unchanged_source, source_matches_stamp};
use crate::source_stamp::{SourceStamp, open_regular_file};
use crate::spill::{SpillFile, SpillWorkspace};
use crate::storage::{available_space, storage_error};
use crate::{
    CompletedIndex, Dialect, Encoding, HeaderMode, IndexConfig, OpenMetrics, OpenOptions,
    QuarryError, Row, Session, StructuralIndex, WorkingCopyCleanup,
};

#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub mod test_pause;

/// Bytes read from a compressed source per buffered read.
const COMPRESSED_READ_BUFFER: usize = 256 * 1024;
/// A tar header is 512 bytes: its checksum field sits at 148..156 and its magic
/// at 257..263.
const TAR_PROBE_BYTES: usize = 512;
const TAR_CHECKSUM: std::ops::Range<usize> = 148..156;
const TAR_MAGIC: std::ops::Range<usize> = 257..263;
const DEFAULT_FREE_SPACE_RESERVE: u64 = 256 * 1024 * 1024;
const DEFAULT_SPACE_CHECK_INTERVAL: u64 = 64 * 1024 * 1024;
/// Compressed bytes the synchronous bootstrap may read beyond the decompressed
/// bytes it has produced. Real data never comes near it; it bounds input that
/// decompresses to almost nothing, such as a long run of empty gzip members.
const DEFAULT_BOOTSTRAP_COMPRESSED_BUDGET: u64 = 16 * 1024 * 1024;

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const GZIP_DEFLATE: u8 = 8;
const FLAG_HCRC: u8 = 0x02;
const FLAG_EXTRA: u8 = 0x04;
const FLAG_NAME: u8 = 0x08;
const FLAG_COMMENT: u8 = 0x10;
const FLAG_RESERVED: u8 = 0xe0;
/// RFC 1952 leaves FNAME and FCOMMENT unbounded. Each is capped so a damaged or
/// hostile header cannot make the reader scan the whole file for a NUL byte.
const MAX_GZIP_HEADER_TEXT: usize = 64 * 1024;
/// Header text bytes read between cancellation checks.
const HEADER_CANCEL_CHECK_BYTES: usize = 4 * 1024;
const TRAILING_DATA: &str = "Unexpected data follows the end of the gzip stream; the file is corrupt or not entirely gzip data.";

/// Compressed source formats that Quarry can open read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionFormat {
    Gzip,
}

impl CompressionFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Gzip => "gz",
        }
    }

    /// Suggest an uncompressed sibling: `sales.csv.gz` becomes `sales.csv`, and a
    /// bare `data.gz` becomes `data.csv`. A name without the compressed extension
    /// gains `-uncompressed`, so the suggestion never names the source itself.
    pub fn suggested_uncompressed_path(self, path: &Path) -> PathBuf {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let suffix = format!(".{}", self.extension());
        let stripped = (name.len() > suffix.len()
            && name.is_char_boundary(name.len() - suffix.len())
            && name[name.len() - suffix.len()..].eq_ignore_ascii_case(&suffix))
        .then(|| name[..name.len() - suffix.len()].to_owned());
        let file_name = match stripped {
            Some(stem) if Path::new(&stem).extension().is_some() => stem,
            Some(stem) => format!("{stem}.csv"),
            None => {
                let base = Path::new(&name);
                let stem = base
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .filter(|stem| !stem.is_empty())
                    .unwrap_or_else(|| "data".to_owned());
                let extension = base
                    .extension()
                    .map(|extension| extension.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "csv".to_owned());
                format!("{stem}-uncompressed.{extension}")
            }
        };
        path.with_file_name(file_name)
    }
}

impl fmt::Display for CompressionFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

fn compressed_error(path: &Path, reason: impl Into<String>) -> QuarryError {
    QuarryError::CompressedFile {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

fn unsupported_format(path: &Path, format: &str) -> QuarryError {
    compressed_error(
        path,
        format!(
            "{format} files are not supported. Quarry currently opens gzip (.gz) compressed files; decompress this file or recompress it with gzip first."
        ),
    )
}

fn tar_archive(path: &Path) -> QuarryError {
    compressed_error(
        path,
        "This is a tar archive. Quarry opens a single gzip-compressed (.gz) delimited file, not archives; extract the file first.",
    )
}

fn lowercase_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Detect a compressed source by signature and name. `Ok(None)` is an ordinary
/// file; unsupported or inconsistent compressed inputs are explicit errors.
pub fn detect_compression(path: &Path) -> Result<Option<CompressionFormat>, QuarryError> {
    let mut file = open_regular_file(path)?;
    compression_of(&mut file, path)
}

/// Classify an already-open file and restore its position to the start.
pub(crate) fn compression_of(
    file: &mut File,
    path: &Path,
) -> Result<Option<CompressionFormat>, QuarryError> {
    file.seek(SeekFrom::Start(0))?;
    let mut prefix = [0_u8; 12];
    let mut used = 0;
    while used < prefix.len() {
        let read = match file.read(&mut prefix[used..]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        used += read;
    }
    file.seek(SeekFrom::Start(0))?;
    classify(path, &prefix[..used])
}

fn classify(path: &Path, prefix: &[u8]) -> Result<Option<CompressionFormat>, QuarryError> {
    let name = lowercase_name(path);
    let tar_name = name.ends_with(".tar.gz") || name.ends_with(".tgz");
    if prefix.starts_with(&GZIP_MAGIC) {
        if tar_name {
            return Err(tar_archive(path));
        }
        return match prefix.get(2) {
            Some(&GZIP_DEFLATE) => Ok(Some(CompressionFormat::Gzip)),
            Some(method) => Err(compressed_error(
                path,
                format!(
                    "This gzip file uses unsupported compression method {method}. Quarry supports standard deflate-compressed gzip (.gz) files."
                ),
            )),
            None => Err(compressed_error(
                path,
                "The gzip header is truncated; the file is incomplete or corrupt.",
            )),
        };
    }
    if prefix.starts_with(b"PK\x03\x04")
        || prefix.starts_with(b"PK\x05\x06")
        || prefix.starts_with(b"PK\x07\x08")
    {
        return Err(unsupported_format(path, "ZIP (.zip)"));
    }
    if prefix.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Err(unsupported_format(path, "Zstandard (.zst)"));
    }
    if prefix.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        return Err(unsupported_format(path, "XZ (.xz)"));
    }
    // "BZh1" is ordinary text, so also require a bzip2 block or end-of-stream magic.
    if prefix.len() >= 10
        && prefix.starts_with(b"BZh")
        && (b'1'..=b'9').contains(&prefix[3])
        && (prefix[4..10] == [0x31, 0x41, 0x59, 0x26, 0x53, 0x59]
            || prefix[4..10] == [0x17, 0x72, 0x45, 0x38, 0x50, 0x90])
    {
        return Err(unsupported_format(path, "bzip2 (.bz2)"));
    }
    if tar_name {
        return Err(tar_archive(path));
    }
    if name.ends_with(".gz") {
        return Err(compressed_error(
            path,
            "This file is named .gz but does not start with a gzip signature. It may be corrupt or renamed.",
        ));
    }
    if name.ends_with(".zst") || name.ends_with(".zstd") {
        return Err(unsupported_format(path, "Zstandard (.zst)"));
    }
    if name.ends_with(".zip") {
        return Err(unsupported_format(path, "ZIP (.zip)"));
    }
    if name.ends_with(".bz2") {
        return Err(unsupported_format(path, "bzip2 (.bz2)"));
    }
    if name.ends_with(".xz") {
        return Err(unsupported_format(path, "XZ (.xz)"));
    }
    Ok(None)
}

/// Explain why an operation that reads saved files cannot use this input yet.
/// I/O failures are left for the operation's own open to report.
pub(crate) fn compressed_input_problem(path: &Path, operation: &str) -> Option<String> {
    input_problem(detect_compression(path), operation)
}

/// Like [`compressed_input_problem`], for an input the operation has already
/// opened: the same handle it then reads is classified, so the file cannot be
/// replaced between the check and the read. `path` is the name the user chose,
/// which decides whether a `.gz` suffix without gzip bytes is rejected. An I/O
/// failure is returned, because the handle's position is then unknown.
pub(crate) fn compressed_file_problem(
    file: &mut File,
    path: &Path,
    operation: &str,
) -> Result<Option<String>, QuarryError> {
    match compression_of(file, path) {
        Err(QuarryError::Io(error)) => Err(QuarryError::Io(error)),
        detected => Ok(input_problem(detected, operation)),
    }
}

fn input_problem(
    detected: Result<Option<CompressionFormat>, QuarryError>,
    operation: &str,
) -> Option<String> {
    match detected {
        Ok(None) | Err(QuarryError::Io(_)) => None,
        Ok(Some(format)) => Some(format!(
            "Compressed {format} (.{}) files are not supported in {operation} yet. Open the file in Quarry and use Save Copy As to write an uncompressed copy first.",
            format.extension()
        )),
        Err(QuarryError::CompressedFile { reason, .. }) => Some(reason),
        Err(error) => Some(error.to_string()),
    }
}

/// True when a destination name would claim compressed output (`.gz` or `.tgz`,
/// in any letter case).
pub fn is_compressed_destination(path: &Path) -> bool {
    let name = lowercase_name(path);
    name.ends_with(".gz") || name.ends_with(".tgz")
}

/// Every Quarry writer produces uncompressed delimited text, so no output may be
/// named as if it were gzip. Apps and the CLI use this rule to pre-check names.
pub fn reject_compressed_destination(path: &Path) -> Result<(), QuarryError> {
    if is_compressed_destination(path) {
        return Err(QuarryError::InvalidOption(
            "Quarry writes uncompressed delimited files; choose a filename without .gz or .tgz",
        ));
    }
    Ok(())
}

/// A complete POSIX or GNU tar header: the `ustar` magic and a checksum that
/// matches the header. Delimited text that merely contains `ustar` at offset 257
/// is not mistaken for an archive.
fn looks_like_tar(prefix: &[u8]) -> bool {
    let Some(header) = prefix.get(..TAR_PROBE_BYTES) else {
        return false;
    };
    if !matches!(&header[TAR_MAGIC], b"ustar\0" | b"ustar ") {
        return false;
    }
    let Some(stored) = tar_octal(&header[TAR_CHECKSUM]) else {
        return false;
    };
    // The checksum counts its own field as eight ASCII spaces.
    let sum: u64 = header
        .iter()
        .enumerate()
        .map(|(offset, byte)| {
            if TAR_CHECKSUM.contains(&offset) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum();
    sum == stored
}

/// A tar numeric field: optional leading spaces, octal digits, then NUL or space.
fn tar_octal(field: &[u8]) -> Option<u64> {
    let mut value = 0_u64;
    let mut digits = 0;
    for byte in field.iter().skip_while(|byte| **byte == b' ') {
        match byte {
            b'0'..=b'7' => {
                value = value.checked_mul(8)? + u64::from(byte - b'0');
                digits += 1;
            }
            0 | b' ' => break,
            _ => return None,
        }
    }
    (digits > 0).then_some(value)
}

/// Marks a gzip integrity failure apart from ordinary I/O errors.
#[derive(Debug)]
struct CorruptGzip(String);

impl fmt::Display for CorruptGzip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CorruptGzip {}

fn corrupt(reason: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, CorruptGzip(reason.into()))
}

/// Marks a read that stopped because its job was cancelled.
#[derive(Debug)]
struct GzipCancelled;

impl fmt::Display for GzipCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("gzip decompression was cancelled")
    }
}

impl std::error::Error for GzipCancelled {}

/// Not `Interrupted`: `read_exact` and `read_full` would retry that kind forever.
fn cancelled() -> io::Error {
    io::Error::other(GzipCancelled)
}

fn is_cancelled(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<GzipCancelled>())
}

/// Marks a read that stopped because the bootstrap's compressed-input budget ran out.
#[derive(Debug)]
struct BudgetSpent;

impl fmt::Display for BudgetSpent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the compressed bootstrap budget was spent")
    }
}

impl std::error::Error for BudgetSpent {}

fn is_budget_spent(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<BudgetSpent>())
}

/// Convert a decoder failure into a path-bearing error, keeping real I/O errors.
fn decode_error(path: &Path, error: io::Error) -> QuarryError {
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<CorruptGzip>())
    {
        Some(reason) => compressed_error(path, reason.0.clone()),
        None => error.into(),
    }
}

/// Counts compressed bytes as they leave the file, for progress reporting. With a
/// limit, it reads no further than that many bytes in total.
struct CountingReader<R> {
    inner: R,
    count: Arc<AtomicU64>,
    limit: Option<u64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let buffer = match self.limit {
            Some(limit) => {
                let remaining = limit.saturating_sub(self.count.load(Ordering::Relaxed));
                if remaining == 0 && !buffer.is_empty() {
                    return Err(io::Error::other(BudgetSpent));
                }
                let allowed = usize::try_from(remaining).unwrap_or(usize::MAX);
                let allowed = buffer.len().min(allowed);
                &mut buffer[..allowed]
            }
            None => buffer,
        };
        let read = self.inner.read(buffer)?;
        self.count.fetch_add(read as u64, Ordering::Relaxed);
        Ok(read)
    }
}

type SourceReader = BufReader<CountingReader<File>>;

fn source_reader(
    file: File,
    count: Arc<AtomicU64>,
    cancel: Option<Arc<AtomicBool>>,
) -> GzipMembers<SourceReader> {
    let mut reader = GzipMembers::new(BufReader::with_capacity(
        COMPRESSED_READ_BUFFER,
        CountingReader {
            inner: file,
            count,
            limit: None,
        },
    ));
    reader.cancel = cancel;
    reader
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberState {
    Header,
    Body,
    End,
}

/// A strict RFC 1952 reader over one or more concatenated gzip members.
///
/// flate2 1.1.9's `MultiGzDecoder` rejects the same damage, but reports
/// truncation and trailing non-gzip bytes as a generic `UnexpectedEof` that
/// cannot be told apart from a failing disk. This reader drives `Decompress`
/// directly so every integrity failure carries a specific `CorruptGzip` reason:
/// it requires a stream end and a matching trailer for every member, and allows
/// nothing but another complete member after it. Header text fields are bounded,
/// and an optional cancellation flag is checked before every inflate step, at
/// every member boundary and while reading long header fields.
pub(crate) struct GzipMembers<R> {
    input: R,
    inflater: Decompress,
    crc: Crc,
    member_bytes: u64,
    members: u64,
    state: MemberState,
    cancel: Option<Arc<AtomicBool>>,
}

impl<R: BufRead> GzipMembers<R> {
    pub(crate) fn new(input: R) -> Self {
        Self {
            input,
            inflater: Decompress::new(false),
            crc: Crc::new(),
            member_bytes: 0,
            members: 0,
            state: MemberState::Header,
            cancel: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
        {
            return Err(cancelled());
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn members(&self) -> u64 {
        self.members
    }

    fn read_header(&mut self) -> io::Result<()> {
        self.check_cancelled()?;
        let mut header_crc = Crc::new();
        let mut fixed = [0_u8; 10];
        header_bytes(&mut self.input, &mut fixed, &mut header_crc)?;
        if fixed[..2] != GZIP_MAGIC {
            return Err(corrupt(if self.members == 0 {
                "The gzip signature is missing; the file is not valid gzip data."
            } else {
                TRAILING_DATA
            }));
        }
        if fixed[2] != GZIP_DEFLATE {
            return Err(corrupt(format!(
                "A gzip member uses unsupported compression method {}.",
                fixed[2]
            )));
        }
        let flags = fixed[3];
        if flags & FLAG_RESERVED != 0 {
            return Err(corrupt(
                "A gzip header sets reserved flags; the file is corrupt.",
            ));
        }
        if flags & FLAG_EXTRA != 0 {
            let mut length = [0_u8; 2];
            header_bytes(&mut self.input, &mut length, &mut header_crc)?;
            let mut remaining = usize::from(u16::from_le_bytes(length));
            let mut skip = [0_u8; 256];
            while remaining > 0 {
                self.check_cancelled()?;
                let step = remaining.min(skip.len());
                header_bytes(&mut self.input, &mut skip[..step], &mut header_crc)?;
                remaining -= step;
            }
        }
        for (flag, field) in [(FLAG_NAME, "filename"), (FLAG_COMMENT, "comment")] {
            if flags & flag != 0 {
                let mut byte = [0_u8; 1];
                let mut length = 0_usize;
                loop {
                    header_bytes(&mut self.input, &mut byte, &mut header_crc)?;
                    if byte[0] == 0 {
                        break;
                    }
                    length += 1;
                    if length > MAX_GZIP_HEADER_TEXT {
                        return Err(corrupt(format!(
                            "The gzip header {field} exceeds the {MAX_GZIP_HEADER_TEXT}-byte limit."
                        )));
                    }
                    if length.is_multiple_of(HEADER_CANCEL_CHECK_BYTES) {
                        self.check_cancelled()?;
                    }
                }
            }
        }
        if flags & FLAG_HCRC != 0 {
            let mut stored = [0_u8; 2];
            header_bytes(&mut self.input, &mut stored, &mut Crc::new())?;
            if u16::from_le_bytes(stored) != header_crc.sum() as u16 {
                return Err(corrupt(
                    "A gzip header checksum does not match; the file is corrupt.",
                ));
            }
        }
        Ok(())
    }

    fn finish_member(&mut self) -> io::Result<()> {
        let mut trailer = [0_u8; 8];
        self.input.read_exact(&mut trailer).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                corrupt("The gzip stream is truncated: a member trailer is missing.")
            } else {
                error
            }
        })?;
        check_trailer(trailer, self.crc.sum(), self.member_bytes)
    }
}

fn header_bytes(input: &mut impl Read, buffer: &mut [u8], crc: &mut Crc) -> io::Result<()> {
    input.read_exact(buffer).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            corrupt("A gzip header is truncated; the file is incomplete or corrupt.")
        } else {
            error
        }
    })?;
    crc.update(buffer);
    Ok(())
}

/// ISIZE stores the member length modulo 2^32, so members over 4 GiB still verify.
pub(crate) fn check_trailer(trailer: [u8; 8], crc: u32, member_bytes: u64) -> io::Result<()> {
    let stored_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let stored_size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    if stored_crc != crc {
        return Err(corrupt(
            "The gzip CRC-32 checksum does not match the decompressed data; the file is corrupt.",
        ));
    }
    if stored_size != member_bytes as u32 {
        return Err(corrupt(
            "The gzip length check (ISIZE) does not match the decompressed data; the file is corrupt.",
        ));
    }
    Ok(())
}

impl<R: BufRead> Read for GzipMembers<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if self.state != MemberState::End {
                self.check_cancelled()?;
            }
            match self.state {
                MemberState::End => return Ok(0),
                MemberState::Header => {
                    if self.members > 0 {
                        let next = self.input.fill_buf()?;
                        if next.is_empty() {
                            self.state = MemberState::End;
                            return Ok(0);
                        }
                        if next[0] != GZIP_MAGIC[0]
                            || next.get(1).is_some_and(|byte| *byte != GZIP_MAGIC[1])
                        {
                            return Err(corrupt(TRAILING_DATA));
                        }
                    }
                    self.read_header()?;
                    self.inflater.reset(false);
                    self.crc.reset();
                    self.member_bytes = 0;
                    self.state = MemberState::Body;
                }
                MemberState::Body => {
                    let input = self.input.fill_buf()?;
                    let at_eof = input.is_empty();
                    let before_in = self.inflater.total_in();
                    let before_out = self.inflater.total_out();
                    let status = self
                        .inflater
                        .decompress(input, output, FlushDecompress::None)
                        .map_err(|_| {
                            corrupt("The compressed data is corrupt (invalid deflate stream).")
                        })?;
                    let consumed = (self.inflater.total_in() - before_in) as usize;
                    let produced = (self.inflater.total_out() - before_out) as usize;
                    self.input.consume(consumed);
                    self.crc.update(&output[..produced]);
                    self.member_bytes += produced as u64;
                    if status == Status::StreamEnd {
                        self.finish_member()?;
                        self.members += 1;
                        self.state = MemberState::Header;
                    } else if produced == 0 {
                        if at_eof {
                            return Err(corrupt(
                                "The gzip stream is truncated: the compressed data ends early.",
                            ));
                        }
                        if consumed == 0 {
                            return Err(corrupt(
                                "The compressed data is corrupt (the deflate stream cannot progress).",
                            ));
                        }
                        continue;
                    }
                    if produced > 0 {
                        return Ok(produced);
                    }
                }
            }
        }
    }
}

/// Fill a buffer completely unless the stream ends first.
fn read_full(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// An in-memory, lazily filled view of the first decompressed bytes. It never
/// writes to disk and holds at most the bootstrap limit plus one lookahead byte,
/// which tells a stream that ends exactly at the limit from a longer one.
struct BootstrapStream<'a> {
    path: &'a Path,
    decoder: GzipMembers<SourceReader>,
    bytes: Vec<u8>,
    position: usize,
    /// The bootstrap limit plus the lookahead byte.
    capacity: usize,
    max_decompressed_bytes: Option<u64>,
    eof: bool,
    failure: Option<QuarryError>,
    /// Compressed bytes that may be read beyond the decompressed bytes produced.
    compressed_budget: u64,
    /// The budget ran out: the bytes so far are all that is read here, and the
    /// end of the stream is left for the background pass to find and verify.
    budget_spent: bool,
}

impl BootstrapStream<'_> {
    fn fail(&mut self, error: QuarryError) -> io::Error {
        let message = error.to_string();
        self.failure = Some(error);
        io::Error::other(message)
    }

    fn fill_to(&mut self, wanted: usize) -> io::Result<()> {
        let wanted = wanted.min(self.capacity);
        while !self.eof && !self.budget_spent && self.bytes.len() < wanted {
            let start = self.bytes.len();
            let step = (wanted - start).min(crate::DEFAULT_READ_CHUNK);
            self.bytes.resize(start + step, 0);
            self.decoder.input.get_mut().limit =
                Some((start as u64).saturating_add(self.compressed_budget));
            let mut read = 0;
            while read < step {
                match self.decoder.read(&mut self.bytes[start + read..]) {
                    Ok(0) => {
                        self.eof = true;
                        break;
                    }
                    Ok(count) => read += count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    // Keep what was decompressed; the decoder is not read again.
                    Err(error) if is_budget_spent(&error) => {
                        self.budget_spent = true;
                        break;
                    }
                    Err(error) => {
                        self.bytes.truncate(start);
                        let error = decode_error(self.path, error);
                        return Err(self.fail(error));
                    }
                }
            }
            self.bytes.truncate(start + read);
            if let Some(limit) = self.max_decompressed_bytes
                && self.bytes.len() as u64 > limit
            {
                let error = QuarryError::DecompressionLimitExceeded {
                    path: self.path.to_path_buf(),
                    limit_bytes: limit,
                    decompressed_bytes: self.bytes.len() as u64,
                };
                return Err(self.fail(error));
            }
        }
        Ok(())
    }
}

impl Read for BootstrapStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.position >= self.bytes.len() {
            self.fill_to(self.position.saturating_add(buffer.len()))?;
        }
        if self.position >= self.bytes.len() {
            if self.eof {
                return Ok(0);
            }
            return Err(io::Error::other(
                "compressed bootstrap read beyond its configured limit",
            ));
        }
        let available = &self.bytes[self.position..];
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.position += count;
        Ok(count)
    }
}

impl Seek for BootstrapStream<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(delta) => self
                .position
                .checked_add_signed(delta as isize)
                .ok_or_else(|| io::Error::other("invalid bootstrap seek"))?
                as u64,
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "a compressed stream has no known end",
                ));
            }
        };
        self.position =
            usize::try_from(target).map_err(|_| io::Error::other("invalid bootstrap seek"))?;
        Ok(target)
    }
}

/// The first rows of a compressed source, read without writing to disk.
#[derive(Debug)]
pub struct CompressedBootstrap {
    path: PathBuf,
    format: CompressionFormat,
    source_stamp: SourceStamp,
    encoding: Encoding,
    compressed_bytes_read: u64,
    decompressed_size: Option<u64>,
    pub dialect: Dialect,
    pub first_rows: Vec<Row>,
    pub metrics: OpenMetrics,
}

impl CompressedBootstrap {
    /// Decompress at most `options.bootstrap_limit` bytes in memory, detect the
    /// text format and materialize the first rows. When the whole stream ends
    /// within that bound, its complete gzip integrity is verified before success.
    /// The compressed input read here is bounded too; past that budget, the rest
    /// of the stream is left for the cancellable background pass.
    pub fn open(
        path: impl AsRef<Path>,
        options: OpenOptions,
        requested_encoding: Option<Encoding>,
        max_decompressed_bytes: Option<u64>,
    ) -> Result<Self, QuarryError> {
        Self::open_with_budget(
            path,
            options,
            requested_encoding,
            max_decompressed_bytes,
            DEFAULT_BOOTSTRAP_COMPRESSED_BUDGET,
        )
    }

    /// Like [`Self::open`], reading at most `compressed_budget` compressed bytes
    /// beyond the decompressed bytes produced.
    pub(crate) fn open_with_budget(
        path: impl AsRef<Path>,
        options: OpenOptions,
        requested_encoding: Option<Encoding>,
        max_decompressed_bytes: Option<u64>,
        compressed_budget: u64,
    ) -> Result<Self, QuarryError> {
        crate::validate_open_options(&options)?;
        if max_decompressed_bytes == Some(0) {
            return Err(QuarryError::InvalidOption(
                "decompressed size limit must be non-zero",
            ));
        }
        let path = path.as_ref().to_path_buf();
        let started = Instant::now();
        let mut file = open_regular_file(&path)?;
        let file_open = started.elapsed();
        let source_stamp = SourceStamp::from_file(&file)?;
        let format = compression_of(&mut file, &path)?.ok_or_else(|| {
            compressed_error(
                &path,
                "This file is not compressed. Open it as an ordinary delimited file.",
            )
        })?;
        let counter = Arc::new(AtomicU64::new(0));
        let mut stream = BootstrapStream {
            path: &path,
            decoder: source_reader(file, Arc::clone(&counter), None),
            bytes: Vec::new(),
            position: 0,
            capacity: options.bootstrap_limit.saturating_add(1),
            max_decompressed_bytes,
            eof: false,
            failure: None,
            compressed_budget,
            budget_spent: false,
        };
        let opened = bootstrap_text(&mut stream, options, requested_encoding);
        let (encoding, dialect, first_rows, bootstrap_bytes) = match opened {
            Ok(opened) => opened,
            Err(error) => return Err(stream.failure.take().unwrap_or(error)),
        };
        // Learn cheaply whether the data ended exactly here; this also verifies
        // the final trailer and rejects trailing bytes for small files.
        if !stream.eof && stream.bytes.len() < stream.capacity {
            let wanted = stream.bytes.len() + 1;
            if let Err(error) = stream.fill_to(wanted) {
                return Err(stream
                    .failure
                    .take()
                    .unwrap_or_else(|| QuarryError::from(error)));
            }
        }
        if !source_matches_stamp(&stream.decoder.input.get_ref().inner, &path, &source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        Ok(Self {
            decompressed_size: stream.eof.then_some(stream.bytes.len() as u64),
            compressed_bytes_read: counter.load(Ordering::Relaxed),
            path,
            format,
            source_stamp,
            encoding,
            dialect,
            first_rows,
            metrics: OpenMetrics {
                file_open,
                first_rows: started.elapsed(),
                bootstrap_bytes_read: bootstrap_bytes,
            },
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn format(&self) -> CompressionFormat {
        self.format
    }

    pub fn compressed_size(&self) -> u64 {
        self.source_stamp.file_size()
    }

    /// Compressed bytes consumed while bootstrapping, including read-ahead.
    pub fn compressed_bytes_read(&self) -> u64 {
        self.compressed_bytes_read
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// True when the complete stream was decompressed and verified during bootstrap.
    pub fn reached_eof(&self) -> bool {
        self.decompressed_size.is_some()
    }

    /// The exact decompressed size, known only when the bootstrap reached the end.
    pub fn decompressed_size(&self) -> Option<u64> {
        self.decompressed_size
    }

    /// A session over the bootstrap rows alone, for showing them before (or instead
    /// of) the private copy. It names no readable file, so every operation that would
    /// read data fails, and in-place saves are rejected as for any compressed document.
    pub fn preview_session(&self) -> Session {
        Session {
            path: PathBuf::new(),
            source_stamp: self.source_stamp.clone(),
            encoding: self.encoding,
            completed_record_count: Arc::new(Mutex::new(None)),
            compressed: Some(Arc::new(CompressedBacking {
                format: self.format,
                source_path: self.path.clone(),
                source_stamp: self.source_stamp.clone(),
                workspace: None,
                _file: None,
                cleanup: WorkingCopyCleanup::default(),
            })),
            section: None,
            file_size: 0,
            dialect: self.dialect,
            first_rows: self.first_rows.clone(),
            metrics: self.metrics,
        }
    }

    /// Start the single background pass that writes the private copy and its index.
    pub fn start_decompression(
        &self,
        config: CompressedOpenConfig,
        cleanup: WorkingCopyCleanup,
    ) -> Result<CompressedDecompressJob, QuarryError> {
        CompressedDecompressJob::start(self, config, cleanup)
    }
}

/// Detect the delimiter and read the first records from the bootstrap stream.
fn leading_records(
    stream: &mut BootstrapStream<'_>,
    options: OpenOptions,
    encoding: Encoding,
) -> Result<(u8, Vec<u8>, Vec<u64>), QuarryError> {
    let delimiter = match options.delimiter {
        Some(delimiter) => {
            RecordScanner::new(delimiter, encoding)?;
            delimiter
        }
        None => crate::detect_file_delimiter(
            stream,
            None,
            options.sample_bytes,
            options.bootstrap_limit,
            encoding,
        )?,
    };
    stream.seek(SeekFrom::Start(0))?;
    let (bootstrap, ends) = crate::read_initial_records(
        stream,
        None,
        delimiter,
        options.rows,
        options.bootstrap_limit,
        encoding,
    )?;
    Ok((delimiter, bootstrap, ends))
}

/// The compressed budget ran out before the first rows were complete: show the
/// complete records decompressed so far, and fail when there are none.
fn budget_records(
    stream: &BootstrapStream<'_>,
    options: OpenOptions,
    encoding: Encoding,
) -> Result<(u8, Vec<u8>, Vec<u64>), QuarryError> {
    let no_rows = || {
        compressed_error(
            stream.path,
            "No complete row was found near the start of the compressed data. The file holds too much compressed data that decompresses to little or nothing, such as many empty gzip members.",
        )
    };
    let mut buffered = &stream.bytes[..stream.bytes.len().min(options.bootstrap_limit)];
    if encoding.is_utf16() {
        buffered = &buffered[..buffered.len() & !1];
    }
    let delimiter = match options.delimiter {
        Some(delimiter) => delimiter,
        None => crate::detect_delimiter(buffered, false, encoding).ok_or_else(no_rows)?,
    };
    let mut scanner = RecordScanner::new(delimiter, encoding)?;
    let mut ends = Vec::new();
    scanner.scan_chunk(buffered, 0, |end| {
        if ends.len() < options.rows {
            ends.push(end);
        }
    })?;
    let last = *ends.last().ok_or_else(no_rows)?;
    Ok((delimiter, buffered[..last as usize].to_vec(), ends))
}

fn bootstrap_text(
    stream: &mut BootstrapStream<'_>,
    options: OpenOptions,
    requested_encoding: Option<Encoding>,
) -> Result<(Encoding, Dialect, Vec<Row>, u64), QuarryError> {
    stream.fill_to(TAR_PROBE_BYTES)?;
    if looks_like_tar(&stream.bytes) {
        return Err(tar_archive(stream.path));
    }
    let prefix = &stream.bytes[..stream.bytes.len().min(4)];
    let encoding = Encoding::detect_prefix(prefix, requested_encoding)?;
    if encoding.is_utf16() && options.bootstrap_limit < 2 {
        return Err(QuarryError::InvalidOption(
            "UTF-16 bootstrap limit must fit a code unit",
        ));
    }
    let (delimiter, bootstrap, ends) = match leading_records(stream, options, encoding) {
        Err(_) if stream.budget_spent && stream.failure.is_none() => {
            budget_records(stream, options, encoding)?
        }
        result => result?,
    };
    let first_rows = crate::materialize_rows(
        &bootstrap,
        &ends[..ends.len().min(options.rows)],
        delimiter,
        encoding,
    )?;
    let has_header = match options.header_mode {
        HeaderMode::Auto => crate::detect_header(&first_rows),
        HeaderMode::FirstRow => true,
        HeaderMode::NoHeader => false,
    };
    Ok((
        encoding,
        Dialect {
            delimiter,
            has_header,
        },
        first_rows,
        bootstrap.len() as u64,
    ))
}

/// Resource bounds for the background decompression pass.
#[derive(Debug, Clone)]
pub struct CompressedOpenConfig {
    /// Folder that holds the private decompressed copy.
    pub temp_dir: PathBuf,
    /// Stop before writing more decompressed bytes than this.
    pub max_decompressed_bytes: Option<u64>,
    /// Keep at least this many bytes free in `temp_dir`. Starting requires the
    /// reserve plus the exact decompressed size when the bootstrap verified it, or
    /// the reserve alone otherwise. While writing, the reserve plus the next chunk
    /// must be free before the first write, after every check interval, and
    /// whenever the next chunk exceeds the headroom that the last check found.
    pub min_free_space_reserve_bytes: u64,
    /// How many decompressed bytes may be written between free-space checks.
    pub space_check_interval_bytes: u64,
    pub index_config: IndexConfig,
    /// Free space that tests report instead of the volume's, before the copy
    /// writes anything; the copy's own writes then use it up.
    #[cfg(test)]
    pub(crate) available_space_override: Option<u64>,
}

impl CompressedOpenConfig {
    pub fn new(temp_dir: impl Into<PathBuf>) -> Self {
        Self {
            temp_dir: temp_dir.into(),
            max_decompressed_bytes: None,
            min_free_space_reserve_bytes: DEFAULT_FREE_SPACE_RESERVE,
            space_check_interval_bytes: DEFAULT_SPACE_CHECK_INTERVAL,
            index_config: IndexConfig::default(),
            #[cfg(test)]
            available_space_override: None,
        }
    }
}

impl Default for CompressedOpenConfig {
    fn default() -> Self {
        Self::new(std::env::temp_dir())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CompressedProgress {
    pub compressed_bytes_read: u64,
    pub compressed_total_bytes: u64,
    pub decompressed_bytes_written: u64,
    pub rows_scanned: u64,
    pub columns_found: usize,
    pub elapsed: Duration,
    pub done: bool,
    pub cancelled: bool,
}

// Returned once per open; boxing the session would only add an indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum CompressedOpenOutcome {
    /// A read-only document over the verified private copy, already indexed.
    Complete {
        session: Session,
        index: StructuralIndex,
    },
    Cancelled,
}

/// Owns the private decompressed copy for as long as any session uses it.
pub(crate) struct CompressedBacking {
    format: CompressionFormat,
    source_path: PathBuf,
    source_stamp: SourceStamp,
    workspace: Option<SpillWorkspace>,
    /// None for a preview session, which has no private copy.
    _file: Option<SpillFile>,
    cleanup: WorkingCopyCleanup,
}

impl fmt::Debug for CompressedBacking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompressedBacking")
            .field("format", &self.format)
            .field("source_path", &self.source_path)
            .finish_non_exhaustive()
    }
}

impl CompressedBacking {
    pub(crate) fn format(&self) -> CompressionFormat {
        self.format
    }

    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub(crate) fn compressed_size(&self) -> u64 {
        self.source_stamp.file_size()
    }

    pub(crate) fn ensure_source_unchanged(&self) -> Result<(), QuarryError> {
        open_unchanged_source(&self.source_path, &self.source_stamp).map(drop)
    }

    pub(crate) fn source_guard(&self) -> CompressedSourceGuard {
        CompressedSourceGuard {
            path: self.source_path.clone(),
            stamp: self.source_stamp.clone(),
        }
    }
}

/// The compressed file a private copy was made from, as it was then. A writer that
/// reads the copy checks it again just before publishing, so a source replaced
/// while the writer ran never yields output from the old data.
#[derive(Clone, Debug)]
pub struct CompressedSourceGuard {
    path: PathBuf,
    stamp: SourceStamp,
}

impl CompressedSourceGuard {
    /// The compressed file the user opened.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Confirm the compressed file still matches the version decompressed.
    pub fn ensure_unchanged(&self) -> Result<(), QuarryError> {
        open_unchanged_source(&self.path, &self.stamp).map(drop)
    }

    /// Debug builds can hold a writer here, so tests can change the source first.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn pause_before_check(&self) {
        if let Some(hold) = test_pause::hold_for(&self.path) {
            hold.reach(
                test_pause::Reached::BeforeGuardedPublication,
                0,
                &AtomicBool::new(false),
            );
        }
    }
}

impl Drop for CompressedBacking {
    fn drop(&mut self) {
        if let Some(workspace) = self.workspace.take() {
            let root = workspace.path.clone();
            if let Err(error) = workspace.cleanup() {
                self.cleanup.report(format_args!(
                    "{}: {error}. Review the remaining files before removing anything.",
                    root.display()
                ));
            }
        }
    }
}

struct SharedState {
    index: RwLock<StructuralIndex>,
    compressed_bytes_read: Arc<AtomicU64>,
    decompressed_bytes: AtomicU64,
    rows_scanned: AtomicU64,
    columns_found: AtomicUsize,
    finished_nanos: AtomicU64,
    done: AtomicBool,
    /// Shared with the gzip reader, so cancellation also stops long header reads.
    cancelled: Arc<AtomicBool>,
    error: Mutex<Option<String>>,
    started: Instant,
    compressed_total: u64,
    #[cfg(any(test, debug_assertions))]
    pause: Option<test_pause::Hold>,
}

impl SharedState {
    fn new(index: StructuralIndex, compressed_total: u64, source: &Path) -> Arc<Self> {
        #[cfg(not(any(test, debug_assertions)))]
        let _ = source;
        Arc::new(Self {
            index: RwLock::new(index),
            compressed_bytes_read: Arc::new(AtomicU64::new(0)),
            decompressed_bytes: AtomicU64::new(0),
            rows_scanned: AtomicU64::new(0),
            columns_found: AtomicUsize::new(0),
            finished_nanos: AtomicU64::new(0),
            done: AtomicBool::new(false),
            cancelled: Arc::new(AtomicBool::new(false)),
            error: Mutex::new(None),
            started: Instant::now(),
            compressed_total,
            #[cfg(any(test, debug_assertions))]
            pause: test_pause::hold_for(source),
        })
    }

    /// Holds the worker here while a test pause for its source asks for it.
    #[cfg(any(test, debug_assertions))]
    fn pause_at(&self, point: test_pause::Reached, written: u64) {
        if let Some(hold) = &self.pause {
            hold.reach(point, written, &self.cancelled);
        }
    }

    fn progress(&self) -> CompressedProgress {
        // Load `done` first: its Acquire pairs with the worker's final Release, so a
        // poll that sees the job finished also sees its final totals and time.
        let done = self.done.load(Ordering::Acquire);
        let finished_nanos = self.finished_nanos.load(Ordering::Acquire);
        CompressedProgress {
            compressed_bytes_read: self.compressed_bytes_read.load(Ordering::Relaxed),
            compressed_total_bytes: self.compressed_total,
            decompressed_bytes_written: self.decompressed_bytes.load(Ordering::Acquire),
            rows_scanned: self.rows_scanned.load(Ordering::Acquire),
            columns_found: self.columns_found.load(Ordering::Acquire),
            elapsed: if finished_nanos == 0 {
                self.started.elapsed()
            } else {
                Duration::from_nanos(finished_nanos)
            },
            done,
            cancelled: self.cancelled.load(Ordering::Acquire),
        }
    }

    fn record_failure(&self, error: &QuarryError) {
        *self.error.lock().unwrap() = Some(error.to_string());
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

fn validate_copy_config(config: &CompressedOpenConfig) -> Result<(), QuarryError> {
    if config.max_decompressed_bytes == Some(0) {
        return Err(QuarryError::InvalidOption(
            "decompressed size limit must be non-zero",
        ));
    }
    if config.space_check_interval_bytes == 0 {
        return Err(QuarryError::InvalidOption(
            "free-space check interval must be non-zero",
        ));
    }
    Ok(())
}

/// Everything a private-copy worker needs; it owns the private workspace until handoff.
struct CopyWorker {
    source_path: PathBuf,
    source_stamp: SourceStamp,
    source_check: File,
    decoder: GzipMembers<SourceReader>,
    workspace: SpillWorkspace,
    file: SpillFile,
    output: File,
    format: CompressionFormat,
    config: CompressedOpenConfig,
    cleanup: WorkingCopyCleanup,
}

/// A flushed private copy whose size and compressed source were verified.
struct FinishedCopy {
    path: PathBuf,
    stamp: SourceStamp,
    backing: CompressedBacking,
}

impl CopyWorker {
    /// Reserve a private workspace for `source`, which must match `source_stamp`
    /// and be positioned at its start. `required_bytes` must be free in the
    /// temporary folder before anything is written.
    #[allow(clippy::too_many_arguments)]
    fn create(
        source_path: PathBuf,
        source_stamp: SourceStamp,
        source: File,
        format: CompressionFormat,
        config: CompressedOpenConfig,
        cleanup: WorkingCopyCleanup,
        shared: &SharedState,
        required_bytes: u64,
    ) -> Result<Self, QuarryError> {
        let source_check = source.try_clone()?;
        let space = crate::inspect_storage(&config.temp_dir)?;
        #[cfg(test)]
        let space = crate::storage::StorageSpace {
            available_bytes: config
                .available_space_override
                .unwrap_or(space.available_bytes),
            ..space
        };
        space.require(required_bytes)?;
        let workspace = SpillWorkspace::create_in(&config.temp_dir, "compressed")?;
        let (file, writer) = workspace.create_run()?;
        let output = writer
            .into_inner()
            .map_err(|error| storage_error(&workspace.path, error.into_error()))?;
        let decoder = source_reader(
            source,
            Arc::clone(&shared.compressed_bytes_read),
            Some(Arc::clone(&shared.cancelled)),
        );
        Ok(Self {
            source_path,
            source_stamp,
            source_check,
            decoder,
            workspace,
            file,
            output,
            format,
            config,
            cleanup,
        })
    }

    /// Stream the source into the private copy, indexing it in the same pass when
    /// `scan` names a delimiter and encoding. Returns None when cancelled. The
    /// workspace is removed on every path except a successful handoff.
    fn run(
        self,
        scan: Option<(u8, Encoding)>,
        shared: &SharedState,
    ) -> Result<Option<FinishedCopy>, QuarryError> {
        let Self {
            source_path,
            source_stamp,
            source_check,
            mut decoder,
            workspace,
            file,
            mut output,
            format,
            config,
            cleanup,
        } = self;
        let written = (|| {
            let finished = decompress(
                &mut decoder,
                &mut output,
                &source_path,
                &workspace.path,
                scan,
                &config,
                shared,
            )?;
            let Some(written) = finished else {
                return Ok(None);
            };
            output
                .sync_all()
                .map_err(|error| storage_error(&workspace.path, error))?;
            #[cfg(any(test, debug_assertions))]
            shared.pause_at(test_pause::Reached::BeforeSourceCheck, written);
            if !source_matches_stamp(&source_check, &source_path, &source_stamp)? {
                return Err(QuarryError::SourceChanged);
            }
            if shared.cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
            let backing_stamp = SourceStamp::from_file(&output)?;
            if backing_stamp.file_size() != written {
                return Err(QuarryError::SourceChanged);
            }
            Ok(Some(backing_stamp))
        })();
        drop(output);
        drop(decoder);
        let stamp = match written {
            Ok(Some(stamp)) => stamp,
            Ok(None) => {
                drop(file);
                release_workspace(workspace, &cleanup);
                return Ok(None);
            }
            Err(error) => {
                drop(file);
                release_workspace(workspace, &cleanup);
                return Err(error);
            }
        };
        Ok(Some(FinishedCopy {
            path: file.to_path_buf(),
            stamp,
            backing: CompressedBacking {
                format,
                source_path,
                source_stamp,
                workspace: Some(workspace),
                _file: Some(file),
                cleanup,
            },
        }))
    }
}

/// The indexing job's worker: a private copy plus the bootstrap's text format.
struct Worker {
    copy: CopyWorker,
    encoding: Encoding,
    dialect: Dialect,
    first_rows: Vec<Row>,
    metrics: OpenMetrics,
}

/// Background decompression into a private file with single-pass indexing.
pub struct CompressedDecompressJob {
    shared: Arc<SharedState>,
    backing_path: PathBuf,
    delimiter: u8,
    encoding: Encoding,
    handle: Option<JoinHandle<Result<CompressedOpenOutcome, QuarryError>>>,
}

impl CompressedDecompressJob {
    pub fn start(
        bootstrap: &CompressedBootstrap,
        config: CompressedOpenConfig,
        cleanup: WorkingCopyCleanup,
    ) -> Result<Self, QuarryError> {
        let index = StructuralIndex::new(config.index_config)?;
        validate_copy_config(&config)?;
        let source = open_unchanged_source(&bootstrap.path, &bootstrap.source_stamp)?;
        let shared = SharedState::new(index, bootstrap.compressed_size(), &bootstrap.path);
        // A verified size needs that much room up front. Otherwise only the reserve is
        // checked now: a multi-member file can be larger than its output, and the
        // copy checks the reserve again before its first write.
        let reserve = config.min_free_space_reserve_bytes;
        let required_bytes = bootstrap
            .decompressed_size
            .map_or(reserve, |exact| exact.saturating_add(reserve));
        let copy = CopyWorker::create(
            bootstrap.path.clone(),
            bootstrap.source_stamp.clone(),
            source,
            bootstrap.format,
            config,
            cleanup,
            &shared,
            required_bytes,
        )?;
        let backing_path = copy.file.to_path_buf();
        let worker = Worker {
            copy,
            encoding: bootstrap.encoding,
            dialect: bootstrap.dialect,
            first_rows: bootstrap.first_rows.clone(),
            metrics: bootstrap.metrics,
        };
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-decompress".into())
            .spawn(move || {
                let _completion = WorkerCompletion(&worker_state);
                let result = run_worker(worker, &worker_state);
                match &result {
                    Ok(CompressedOpenOutcome::Cancelled) => {
                        worker_state.cancelled.store(true, Ordering::Release);
                    }
                    Err(error) => worker_state.record_failure(error),
                    Ok(CompressedOpenOutcome::Complete { .. }) => {}
                }
                result
            })?;
        Ok(Self {
            shared,
            backing_path,
            delimiter: bootstrap.dialect.delimiter,
            encoding: bootstrap.encoding,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> CompressedProgress {
        self.shared.progress()
    }

    /// The rows indexed so far; every indexed byte is already in the private copy.
    pub fn snapshot(&self) -> StructuralIndex {
        self.shared.index.read().unwrap().clone()
    }

    /// Read rows that are already decompressed and indexed while the job continues.
    /// Reading stops at the published index boundary, so bytes written after it are
    /// never parsed.
    pub fn read_rows(&self, start: u64, count: usize) -> Result<Vec<Row>, QuarryError> {
        let snapshot = self.snapshot();
        crate::read_rows_from_index(
            &self.backing_path,
            self.delimiter,
            &snapshot,
            start,
            count,
            crate::DEFAULT_MAX_RECORD_BYTES,
            self.encoding,
            snapshot.indexed_bytes(),
        )
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
    }

    pub fn wait(mut self) -> Result<CompressedOpenOutcome, QuarryError> {
        self.handle
            .take()
            .expect("decompression handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl Drop for CompressedDecompressJob {
    /// Abandoning the job stops the worker and removes its private copy.
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

/// A verified private decompressed copy of a compressed source, never parsed as
/// delimited text. Dropping it removes the copy and its private folder.
#[derive(Debug)]
pub struct DecompressedCopy {
    path: PathBuf,
    size: u64,
    backing: CompressedBacking,
}

impl DecompressedCopy {
    /// The owner-only private file that holds the decompressed bytes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn decompressed_size(&self) -> u64 {
        self.size
    }

    /// The compressed file the copy was made from.
    pub fn source_path(&self) -> &Path {
        self.backing.source_path()
    }

    pub fn format(&self) -> CompressionFormat {
        self.backing.format()
    }

    pub fn compressed_size(&self) -> u64 {
        self.backing.compressed_size()
    }

    /// Confirm the compressed source still matches the version decompressed.
    pub fn ensure_source_unchanged(&self) -> Result<(), QuarryError> {
        self.backing.ensure_source_unchanged()
    }

    /// A guard that a writer reading this copy checks before it publishes.
    pub fn source_guard(&self) -> CompressedSourceGuard {
        self.backing.source_guard()
    }
}

/// Background decompression of a compressed source into a private copy without
/// parsing it, so tools such as inspection can diagnose malformed delimited data
/// inside a valid gzip file. It applies the same strict gzip validation, size cap,
/// free-space reserve, cancellation, source check and cleanup as
/// [`CompressedDecompressJob`].
pub struct CompressedRawDecompressJob {
    shared: Arc<SharedState>,
    handle: Option<JoinHandle<Result<Option<DecompressedCopy>, QuarryError>>>,
}

impl CompressedRawDecompressJob {
    pub fn start(
        path: impl AsRef<Path>,
        config: CompressedOpenConfig,
        cleanup: WorkingCopyCleanup,
    ) -> Result<Self, QuarryError> {
        let index = StructuralIndex::new(config.index_config)?;
        validate_copy_config(&config)?;
        let path = path.as_ref().to_path_buf();
        let mut source = open_regular_file(&path)?;
        let source_stamp = SourceStamp::from_file(&source)?;
        let format = compression_of(&mut source, &path)?.ok_or_else(|| {
            compressed_error(
                &path,
                "This file is not compressed. Read it directly instead of decompressing it.",
            )
        })?;
        let shared = SharedState::new(index, source_stamp.file_size(), &path);
        // The final size is unknown, so only the reserve is checked before starting.
        let required_bytes = config.min_free_space_reserve_bytes;
        let copy = CopyWorker::create(
            path,
            source_stamp,
            source,
            format,
            config,
            cleanup,
            &shared,
            required_bytes,
        )?;
        let worker_state = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-decompress".into())
            .spawn(move || {
                let _completion = WorkerCompletion(&worker_state);
                let result = copy.run(None, &worker_state).map(|copy| {
                    copy.map(
                        |FinishedCopy {
                             path,
                             stamp,
                             backing,
                         }| DecompressedCopy {
                            path,
                            size: stamp.file_size(),
                            backing,
                        },
                    )
                });
                match &result {
                    Ok(None) => worker_state.cancelled.store(true, Ordering::Release),
                    Err(error) => worker_state.record_failure(error),
                    Ok(Some(_)) => {}
                }
                result
            })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    pub fn progress(&self) -> CompressedProgress {
        self.shared.progress()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
    }

    /// The verified private copy, or None when the job was cancelled.
    pub fn wait(mut self) -> Result<Option<DecompressedCopy>, QuarryError> {
        self.handle
            .take()
            .expect("decompression handle is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl Drop for CompressedRawDecompressJob {
    /// Abandoning the job stops the worker and removes its private copy.
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

fn release_workspace(workspace: SpillWorkspace, cleanup: &WorkingCopyCleanup) {
    let root = workspace.path.clone();
    if let Err(error) = workspace.cleanup() {
        cleanup.report(format_args!(
            "{}: {error}. Review the remaining files before removing anything.",
            root.display()
        ));
    }
}

fn run_worker(worker: Worker, shared: &SharedState) -> Result<CompressedOpenOutcome, QuarryError> {
    let Worker {
        copy,
        encoding,
        dialect,
        first_rows,
        metrics,
    } = worker;
    let Some(FinishedCopy {
        path,
        stamp,
        backing,
    }) = copy.run(Some((dialect.delimiter, encoding)), shared)?
    else {
        return Ok(CompressedOpenOutcome::Cancelled);
    };
    let file_size = stamp.file_size();
    let session = Session {
        path,
        source_stamp: stamp.clone(),
        encoding,
        completed_record_count: Arc::new(Mutex::new(None)),
        compressed: Some(Arc::new(backing)),
        section: None,
        file_size,
        dialect,
        first_rows,
        metrics,
    };
    let index = shared.index.read().unwrap().clone();
    let completed = CompletedIndex::from_output(index, stamp, dialect.delimiter, encoding)
        .ok_or(QuarryError::SourceChanged)?;
    let index = session.adopt_completed_index(completed)?;
    Ok(CompressedOpenOutcome::Complete { session, index })
}

/// Returns the decompressed length, or None when cancelled. With `scan`, the
/// structural index is built in the same pass.
fn decompress(
    decoder: &mut GzipMembers<SourceReader>,
    output: &mut File,
    source_path: &Path,
    workspace_path: &Path,
    scan: Option<(u8, Encoding)>,
    config: &CompressedOpenConfig,
    shared: &SharedState,
) -> Result<Option<u64>, QuarryError> {
    let mut scanner = scan
        .map(|(delimiter, encoding)| {
            EncodedScanner::<true>::new(delimiter, encoding).map(|scanner| (scanner, encoding))
        })
        .transpose()?;
    let chunk_bytes = match scan {
        Some((_, encoding)) => encoding.chunk_size(config.index_config.chunk_bytes),
        None => config.index_config.chunk_bytes,
    };
    let mut chunk = vec![0; chunk_bytes];
    let mut written = 0_u64;
    // Free space is checked before the first write, once per interval, and whenever
    // the next chunk exceeds the headroom above the reserve that the last check
    // found, so even a copy shorter than one interval keeps the reserve.
    let mut next_space_check = 0_u64;
    let mut headroom = 0_u64;
    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let read = match read_full(decoder, &mut chunk) {
            Ok(read) => read,
            Err(error) if is_cancelled(&error) => return Ok(None),
            Err(error) => return Err(decode_error(source_path, error)),
        };
        if read == 0 {
            break;
        }
        let total = written.saturating_add(read as u64);
        if let Some(limit) = config.max_decompressed_bytes
            && total > limit
        {
            return Err(QuarryError::DecompressionLimitExceeded {
                path: source_path.to_path_buf(),
                limit_bytes: limit,
                decompressed_bytes: total,
            });
        }
        // A chunk larger than the headroom left by the last check is checked again
        // first, so a write never reaches into the reserve between intervals.
        if written >= next_space_check || read as u64 > headroom {
            headroom = ensure_free_space(config, read as u64, written)?;
            next_space_check = written.saturating_add(config.space_check_interval_bytes);
        }
        // Write before indexing: live readers only see rows already on disk.
        output
            .write_all(&chunk[..read])
            .map_err(|error| storage_error(workspace_path, error))?;
        headroom = headroom.saturating_sub(read as u64);
        let chunk_start = written;
        written = total;
        if let Some((scanner, encoding)) = scanner.as_mut() {
            // A short UTF-16 read can only be the final chunk; finish rejects its odd byte.
            let scan_len = if encoding.is_utf16() { read & !1 } else { read };
            let mut index = shared.index.write().unwrap();
            scanner.scan_chunk(&chunk[..scan_len], chunk_start, |end| {
                index.record_boundary(end)
            })?;
            index.set_scan_progress(chunk_start + scan_len as u64, scanner.maximum_width());
            shared
                .columns_found
                .store(index.maximum_width(), Ordering::Release);
            shared
                .rows_scanned
                .store(index.indexed_rows(), Ordering::Release);
            // Publish the byte count before releasing the index, so a reader that
            // sees these rows never sees an older byte count.
            shared.decompressed_bytes.store(written, Ordering::Release);
            drop(index);
        } else {
            shared.decompressed_bytes.store(written, Ordering::Release);
        }
        #[cfg(any(test, debug_assertions))]
        shared.pause_at(test_pause::Reached::Published, written);
    }
    if let Some((scanner, _)) = scanner.as_mut() {
        let mut index = shared.index.write().unwrap();
        scanner.finish(written, |end| index.record_boundary(end))?;
        index.set_scan_progress(written, scanner.maximum_width());
        shared
            .columns_found
            .store(index.maximum_width(), Ordering::Release);
        shared
            .rows_scanned
            .store(index.indexed_rows(), Ordering::Release);
    }
    Ok(Some(written))
}

/// Require the reserve plus `incoming` bytes to be free before writing them, and
/// return the free space above the reserve. `written` is what this copy has
/// already written.
fn ensure_free_space(
    config: &CompressedOpenConfig,
    incoming: u64,
    written: u64,
) -> Result<u64, QuarryError> {
    let directory = &config.temp_dir;
    let available = available_space(directory).map_err(|error| storage_error(directory, error))?;
    #[cfg(test)]
    let available = config
        .available_space_override
        .map_or(available, |space| space.saturating_sub(written));
    #[cfg(not(test))]
    let _ = written;
    let required = config.min_free_space_reserve_bytes.saturating_add(incoming);
    if available < required {
        return Err(QuarryError::InsufficientStorage {
            directory: directory.to_path_buf(),
            required_bytes: required,
            available_bytes: available,
        });
    }
    Ok(available - config.min_free_space_reserve_bytes)
}

impl Session {
    pub fn is_compressed(&self) -> bool {
        self.compressed.is_some()
    }

    pub fn compression(&self) -> Option<CompressionFormat> {
        self.compressed.as_ref().map(|backing| backing.format())
    }

    /// The compressed file the user opened; `path()` names the private copy.
    pub fn compressed_source_path(&self) -> Option<&Path> {
        self.compressed
            .as_ref()
            .map(|backing| backing.source_path())
    }

    pub fn compressed_file_size(&self) -> Option<u64> {
        self.compressed
            .as_ref()
            .map(|backing| backing.compressed_size())
    }

    /// OS file identity of the original compressed source file (if compressed).
    pub fn compressed_source_file_identity(&self) -> Option<crate::FileIdentity> {
        self.compressed
            .as_ref()
            .and_then(|backing| backing.source_stamp.file_identity())
    }

    /// Confirm the compressed source still matches the version decompressed.
    pub fn ensure_compressed_source_unchanged(&self) -> Result<(), QuarryError> {
        match &self.compressed {
            Some(backing) => backing.ensure_source_unchanged(),
            None => Ok(()),
        }
    }

    /// For a compressed document, a guard that a writer reading the private copy
    /// checks before it publishes; None for a plain file.
    pub fn compressed_source_guard(&self) -> Option<CompressedSourceGuard> {
        self.compressed
            .as_ref()
            .map(|backing| backing.source_guard())
    }

    /// Reinterpret the verified private copy with another delimiter, header or
    /// encoding choice without decompressing the source again.
    pub fn reopen_compressed_backing(
        &self,
        options: OpenOptions,
        requested_encoding: Option<Encoding>,
    ) -> Result<Session, QuarryError> {
        let backing = self.compressed.as_ref().ok_or(QuarryError::InvalidOption(
            "only a compressed document can reopen its decompressed copy",
        ))?;
        backing.ensure_source_unchanged()?;
        self.ensure_source_unchanged()?;
        let mut session = Session::open_uncompressed(&self.path, options, requested_encoding)?;
        if !self.source_stamp.matches(&session.source_stamp) {
            return Err(QuarryError::SourceChanged);
        }
        session.compressed = Some(Arc::clone(backing));
        Ok(session)
    }
}

#[cfg(test)]
mod tests;
