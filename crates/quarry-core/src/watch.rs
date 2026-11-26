//! Opt-in append-only Live Watch Mode (ADR 0016).
//!
//! Tracks three separate monotonic frontiers (`observed_file_len`, `scanner_consumed_bytes`,
//! `committed_record_end`), keeps the final unterminated record provisional across arbitrary
//! read splits (trailing `\r`, quote/escape state, split UTF-8, odd UTF-16 byte, pending UTF-16
//! surrogate), verifies the committed tail seam on every growth event, detects truncation /
//! copytruncate / rename rotation (`tail -f` drain + "Open new file"), and evaluates active
//! filters incrementally on appended rows only.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::encoding::{EncodedScanner, Encoding};
use crate::export::source_matches_stamp;
use crate::filter::{
    FilterEdits, FilterIndex, FilterMatch, FilterQuery, compile_matchers, predicate_groups,
    record_matches_fast,
};
use crate::index::StructuralIndex;
use crate::positional_io::read_at_exact_or_eof;
use crate::sidecar::{IndexCacheConfig, is_network_filesystem, save_sidecar};
use crate::source_stamp::{FileIdentity, SourceStamp, open_regular_file};
use crate::{
    DEFAULT_MAX_RECORD_BYTES, DEFAULT_READ_CHUNK, Dialect, OpenMetrics, QuarryError, Row, Session,
    parse_encoded_record_with_bom, read_rows_from_open_file,
};

/// Default local reconciliation wakeup interval paired with native `kqueue` / `inotify` events.
pub const DEFAULT_WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Default bounded polling interval for network filesystems and platforms without native vnode watches.
pub const NETWORK_WATCH_POLL_INTERVAL: Duration = Duration::from_millis(500);

const SEAM_WINDOW_BYTES: usize = 4096;
const PROVISIONAL_PREVIEW_MAX_BYTES: usize = 4096;
const WATCH_READ_CHUNK_BYTES: usize = 256 * 1024;

/// Active OS wakeup mechanism selected for a [`LiveWatchJob`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchBackend {
    Kqueue,
    Inotify,
    Polling,
}

impl WatchBackend {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Kqueue => "kqueue",
            Self::Inotify => "inotify",
            Self::Polling => "poll",
        }
    }

    /// Number of extra OS descriptors owned by the notification backend itself (excluding the
    /// pinned source file descriptor).
    pub const fn extra_fd_count(self) -> usize {
        match self {
            // kqueue/inotify FD + self-pipe read/write pair.
            Self::Kqueue | Self::Inotify => 3,
            Self::Polling => 0,
        }
    }
}

/// Terminal alert explaining why Live Watch stopped automatically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchAlert {
    /// A new file with a different identity appeared at the watched path after draining the old FD.
    Rotated,
    /// The watched file shrank or a positional read returned fewer bytes than observed.
    Truncated { previous_len: u64, current_len: u64 },
    /// The committed tail seam changed in place (overwrite or copytruncate).
    Rewritten,
    /// A parse, encoding, or 64 MiB pending-tail limit error occurred while scanning appended bytes.
    ParseError(String),
    /// An underlying I/O error prevented continuing Live Watch.
    IoError(String),
}

impl WatchAlert {
    pub fn summary(&self) -> String {
        match self {
            Self::Rotated => {
                "File was rotated or replaced on disk. Open the new file to watch it.".into()
            }
            Self::Truncated {
                previous_len,
                current_len,
            } => format!(
                "File was truncated on disk ({previous_len} bytes -> {current_len} bytes). Reload from disk to view the updated file."
            ),
            Self::Rewritten => {
                "File was overwritten in place (tail seam changed). Reload from disk to view the updated file."
                    .into()
            }
            Self::ParseError(reason) => format!("Live Watch stopped: {reason}"),
            Self::IoError(reason) => format!("Live Watch I/O error: {reason}"),
        }
    }
}

/// Lifecycle state of a [`LiveWatchJob`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchStatus {
    /// Actively watching and ingesting appended records.
    Watching,
    /// The path was unlinked or renamed away; still draining appends on the open FD until a new
    /// file appears at the path (which transitions to `Alert(WatchAlert::Rotated)`) or the user stops.
    FollowingUnlinked,
    /// Cleanly stopped by the user.
    Stopped,
    /// Stopped automatically after detecting rotation, truncation, rewrite, or an error.
    Alert(WatchAlert),
}

impl WatchStatus {
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Watching | Self::FollowingUnlinked)
    }
}

/// Uncommitted bytes past the last newline-terminated record boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchProvisionalTail {
    pub offset: u64,
    pub byte_len: u64,
    pub preview_fields: Option<Vec<Vec<u8>>>,
}

/// Point-in-time read-only snapshot of a running or finished [`LiveWatchJob`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchSnapshot {
    pub status: WatchStatus,
    pub backend: WatchBackend,
    pub observed_file_len: u64,
    pub scanner_consumed_bytes: u64,
    pub committed_record_end: u64,
    pub committed_rows: u64,
    pub appended_rows: u64,
    pub maximum_width: usize,
    pub provisional_tail: Option<WatchProvisionalTail>,
    pub filter_matches_found: Option<u64>,
    pub events_coalesced: u64,
    pub generation: u64,
}

/// Configuration for [`Session::start_live_watch`].
#[derive(Clone, Debug)]
pub struct WatchConfig {
    pub poll_interval: Duration,
    pub force_polling: bool,
    pub max_record_bytes: usize,
    pub cache_config: IndexCacheConfig,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_WATCH_POLL_INTERVAL,
            force_polling: false,
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
            cache_config: IndexCacheConfig::default(),
        }
    }
}

/// Reconciled document state returned when [`LiveWatchJob::stop`] finishes.
pub struct WatchStopOutcome {
    pub session: Session,
    pub index: StructuralIndex,
    pub filter_index: Option<FilterIndex>,
    pub final_status: WatchStatus,
    pub snapshot: WatchSnapshot,
}

struct WatchFilterState {
    index: FilterIndex,
    data_start: u64,
    max_column: usize,
    edits: FilterEdits,
}

impl WatchFilterState {
    fn new(index: FilterIndex, data_start: u64) -> Self {
        let max_column = index
            .query()
            .predicates
            .iter()
            .map(|predicate| predicate.column)
            .max()
            .unwrap_or(0);
        Self {
            index,
            data_start,
            max_column,
            edits: Arc::default(),
        }
    }
}

/// Stateful tail scanner tracking the three watch frontiers and carrying all partial record /
/// encoding state across arbitrary write splits.
struct TailScanner {
    delimiter: u8,
    encoding: Encoding,
    max_record_bytes: usize,
    scanner: EncodedScanner<true>,
    pending_utf16_byte: Option<u8>,
    provisional_bytes: Vec<u8>,
    observed_file_len: u64,
    scanner_consumed_bytes: u64,
    committed_record_end: u64,
    seam_bytes: Vec<u8>,
}

impl TailScanner {
    fn new(
        open_file: &File,
        committed_record_end: u64,
        observed_file_len: u64,
        delimiter: u8,
        encoding: Encoding,
        max_record_bytes: usize,
    ) -> Result<Self, QuarryError> {
        let scanner = EncodedScanner::<true>::at_offset_with_bom_handling(
            delimiter,
            committed_record_end,
            encoding,
            true,
        )?;
        let seam_bytes = read_seam_bytes(open_file, committed_record_end)?;
        Ok(Self {
            delimiter,
            encoding,
            max_record_bytes,
            scanner,
            pending_utf16_byte: None,
            provisional_bytes: Vec::new(),
            observed_file_len,
            scanner_consumed_bytes: committed_record_end,
            committed_record_end,
            seam_bytes,
        })
    }

    fn verify_seam(&self, open_file: &File) -> Result<SeamCheck, QuarryError> {
        if !self.seam_bytes.is_empty() {
            let start = self
                .committed_record_end
                .saturating_sub(self.seam_bytes.len() as u64);
            let mut observed = vec![0u8; self.seam_bytes.len()];
            let read = read_at_exact_or_eof(open_file, start, &mut observed)?;
            if read < self.seam_bytes.len() {
                return Ok(SeamCheck::Truncated {
                    current_len: start + read as u64,
                });
            }
            if observed != self.seam_bytes {
                return Ok(SeamCheck::Rewritten);
            }
        }
        // Also verify all retained provisional tail bytes against the pinned descriptor so a
        // same-size in-place rewrite of uncommitted bytes cannot be finalized as stale cached data
        // or published into a valid-looking sidecar.
        if !self.provisional_bytes.is_empty() {
            const VERIFY_CHUNK: usize = 64 * 1024;
            let mut buf = vec![0u8; self.provisional_bytes.len().min(VERIFY_CHUNK)];
            let mut verified = 0usize;
            while verified < self.provisional_bytes.len() {
                let want = (self.provisional_bytes.len() - verified).min(buf.len());
                let offset = self.committed_record_end + verified as u64;
                let read = read_at_exact_or_eof(open_file, offset, &mut buf[..want])?;
                if read < want {
                    return Ok(SeamCheck::Truncated {
                        current_len: offset + read as u64,
                    });
                }
                if buf[..read] != self.provisional_bytes[verified..verified + read] {
                    return Ok(SeamCheck::Rewritten);
                }
                verified += read;
            }
        }
        Ok(SeamCheck::Ok)
    }

    fn append_to_seam(&mut self, committed_slice: &[u8]) {
        append_to_seam_buf(&mut self.seam_bytes, committed_slice);
    }

    /// Consumes `new_bytes` read starting at `self.scanner_consumed_bytes`, committing every
    /// newline-terminated record into `index` and incrementally evaluating `filter` if present.
    fn feed_bytes(
        &mut self,
        new_bytes: &[u8],
        index: &mut StructuralIndex,
        mut filter: Option<&mut WatchFilterState>,
    ) -> Result<u64, QuarryError> {
        if new_bytes.is_empty() {
            return Ok(0);
        }

        // Enforce the 64 MiB uncommitted-tail cap incrementally as bytes arrive.
        let mut newly_committed = 0u64;
        let feed_start_offset = self.scanner_consumed_bytes;
        self.scanner_consumed_bytes = self
            .scanner_consumed_bytes
            .checked_add(new_bytes.len() as u64)
            .ok_or(QuarryError::InvalidOption(
                "file offset overflow during Live Watch",
            ))?;

        // Build the scanner-aligned slice (carrying any odd UTF-16 byte across reads).
        let mut owned_aligned: Vec<u8> = Vec::new();
        let (scan_slice, scan_abs_start): (&[u8], u64) = if self.encoding.is_utf16() {
            let carry_len = usize::from(self.pending_utf16_byte.is_some());
            let total_len = carry_len + new_bytes.len();
            if total_len < 2 {
                // Only 1 byte total so far; keep it in pending_utf16_byte and provisional_bytes.
                self.pending_utf16_byte = Some(new_bytes[0]);
                self.provisional_bytes.push(new_bytes[0]);
                if self.provisional_bytes.len() > self.max_record_bytes {
                    return Err(QuarryError::RecordTooLarge {
                        limit: self.max_record_bytes,
                    });
                }
                return Ok(0);
            }
            let even_len = total_len & !1;
            let abs_start = feed_start_offset - carry_len as u64;
            owned_aligned.reserve(even_len);
            if let Some(prev) = self.pending_utf16_byte.take() {
                owned_aligned.push(prev);
            }
            let take_from_new = even_len - carry_len;
            owned_aligned.extend_from_slice(&new_bytes[..take_from_new]);
            if take_from_new < new_bytes.len() {
                self.pending_utf16_byte = Some(new_bytes[take_from_new]);
            }
            (&owned_aligned, abs_start)
        } else {
            (new_bytes, feed_start_offset)
        };

        // Compile filter matchers once per batch when a filter is active.
        let mut field_ranges: Vec<(usize, usize)> = Vec::new();
        let mut record_ends: Vec<u64> = Vec::new();
        self.scanner
            .scan_chunk(scan_slice, scan_abs_start, |absolute_end| {
                record_ends.push(absolute_end);
            })?;

        let mut consumed_from_new = 0usize;
        if record_ends.is_empty() {
            self.provisional_bytes.extend_from_slice(new_bytes);
            if self.provisional_bytes.len() > self.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: self.max_record_bytes,
                });
            }
            return Ok(0);
        }

        // Pre-compile filter matchers once for all committed records in this chunk.
        let filter_query = filter.as_deref().map(|state| state.index.query().clone());
        let compiled_filter = if let Some(query) = filter_query.as_ref() {
            let matchers = compile_matchers(query)?;
            let groups = predicate_groups(query);
            Some((query, matchers, groups))
        } else {
            None
        };

        // Process each newly terminated record in order so `provisional_bytes` never exceeds the cap.
        for absolute_end in record_ends {
            let local_end_in_new = usize::try_from(
                absolute_end
                    .checked_sub(feed_start_offset)
                    .expect("record end lies at or after new bytes"),
            )
            .expect("chunk record end fits in usize");
            let piece = &new_bytes[consumed_from_new..local_end_in_new];
            let rec_len = self.provisional_bytes.len().saturating_add(piece.len());
            if rec_len > self.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: self.max_record_bytes,
                });
            }

            let record_start = self.committed_record_end;
            let row_number = index.indexed_rows();

            // Borrow directly from `piece` when the entire record lies inside `new_bytes`
            // (zero-copy fast path during sustained streaming!), otherwise assemble in `provisional_bytes`.
            let record_slice: &[u8] = if self.provisional_bytes.is_empty() {
                piece
            } else {
                self.provisional_bytes.extend_from_slice(piece);
                &self.provisional_bytes
            };

            // Validate single-byte legacy encodings (e.g. undefined Windows-1252 bytes) at commit time
            // when needed, and evaluate any active filter incrementally.
            if self.encoding == Encoding::Windows1252 {
                let body = if record_start == 0 {
                    self.encoding.strip_bom(record_slice).1
                } else {
                    record_slice
                };
                self.encoding
                    .decode(body, self.max_record_bytes)
                    .map_err(|err| crate::record_encoding_error(row_number + 1, err))?;
            }

            if let (Some(filter_state), Some((query, matchers, groups))) =
                (filter.as_deref_mut(), compiled_filter.as_ref())
                && row_number >= filter_state.data_start
            {
                let matched = record_matches_fast(
                    record_slice,
                    self.delimiter,
                    row_number,
                    record_start == 0,
                    query,
                    matchers,
                    groups,
                    filter_state.max_column,
                    &filter_state.edits,
                    self.encoding,
                    &mut field_ranges,
                )?;
                if matched {
                    filter_state.index.record_match(row_number, record_start);
                }
            }

            append_to_seam_buf(&mut self.seam_bytes, record_slice);
            index.record_boundary(absolute_end);
            index.set_maximum_width(self.scanner.maximum_width());
            self.committed_record_end = absolute_end;
            self.provisional_bytes.clear();
            consumed_from_new = local_end_in_new;
            newly_committed += 1;
        }

        // Retain any remaining uncommitted tail bytes past the last newline.
        if consumed_from_new < new_bytes.len() {
            let tail = &new_bytes[consumed_from_new..];
            if self.provisional_bytes.len().saturating_add(tail.len()) > self.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: self.max_record_bytes,
                });
            }
            self.provisional_bytes.extend_from_slice(tail);
        }

        Ok(newly_committed)
    }

    /// Finalizes any non-empty provisional EOF record when the user cleanly stops Live Watch.
    fn finish_on_clean_stop(
        &mut self,
        index: &mut StructuralIndex,
        filter: Option<&mut WatchFilterState>,
    ) -> Result<bool, QuarryError> {
        if self.provisional_bytes.is_empty() {
            return Ok(false);
        }
        if self.pending_utf16_byte.is_some() {
            return Err(QuarryError::EncodingAt {
                encoding: self.encoding,
                offset: self.scanner_consumed_bytes.saturating_sub(1),
                reason: "incomplete UTF-16 code unit".into(),
            });
        }
        if self.provisional_bytes.len() > self.max_record_bytes {
            return Err(QuarryError::RecordTooLarge {
                limit: self.max_record_bytes,
            });
        }
        let mut final_end: Option<u64> = None;
        let has_final = self.scanner.finish(self.scanner_consumed_bytes, |end| {
            final_end = Some(end);
        })?;
        if !has_final {
            // e.g., BOM-only file
            self.committed_record_end = self.scanner_consumed_bytes;
            self.provisional_bytes.clear();
            return Ok(false);
        }
        let end = final_end.unwrap_or(self.scanner_consumed_bytes);
        let record_start = self.committed_record_end;
        let row_number = index.indexed_rows();
        if self.encoding == Encoding::Windows1252 {
            let body = if record_start == 0 {
                self.encoding.strip_bom(&self.provisional_bytes).1
            } else {
                &self.provisional_bytes
            };
            self.encoding
                .decode(body, self.max_record_bytes)
                .map_err(|err| crate::record_encoding_error(row_number + 1, err))?;
        }
        if let Some(filter_state) = filter
            && row_number >= filter_state.data_start
        {
            let query = filter_state.index.query().clone();
            let matchers = compile_matchers(&query)?;
            let groups = predicate_groups(&query);
            let mut field_ranges = Vec::new();
            let matched = record_matches_fast(
                &self.provisional_bytes,
                self.delimiter,
                row_number,
                record_start == 0,
                &query,
                &matchers,
                &groups,
                filter_state.max_column,
                &filter_state.edits,
                self.encoding,
                &mut field_ranges,
            )?;
            if matched {
                filter_state.index.record_match(row_number, record_start);
            }
        }
        let seam_copy = self.provisional_bytes.clone();
        self.append_to_seam(&seam_copy);
        index.record_boundary(end);
        index.set_maximum_width(self.scanner.maximum_width());
        self.committed_record_end = end;
        self.provisional_bytes.clear();
        Ok(true)
    }

    fn provisional_tail(&self) -> Option<WatchProvisionalTail> {
        let byte_len = self
            .observed_file_len
            .saturating_sub(self.committed_record_end);
        if byte_len == 0 && self.provisional_bytes.is_empty() {
            return None;
        }
        let actual_len = byte_len.max(self.provisional_bytes.len() as u64);
        let preview_fields = self.try_preview_provisional();
        Some(WatchProvisionalTail {
            offset: self.committed_record_end,
            byte_len: actual_len,
            preview_fields,
        })
    }

    fn try_preview_provisional(&self) -> Option<Vec<Vec<u8>>> {
        if self.provisional_bytes.is_empty()
            || self.provisional_bytes.len() > PROVISIONAL_PREVIEW_MAX_BYTES
            || self.pending_utf16_byte.is_some()
        {
            return None;
        }
        // Avoid showing broken replacement characters while a multi-byte UTF-8 sequence is split.
        if self.encoding == Encoding::Utf8 && std::str::from_utf8(&self.provisional_bytes).is_err()
        {
            return None;
        }
        parse_encoded_record_with_bom(
            &self.provisional_bytes,
            self.delimiter,
            0,
            self.committed_record_end == 0,
            self.encoding,
        )
        .ok()
        .map(|fields| fields.into_iter().map(|f| f.into_owned()).collect())
    }
}

enum SeamCheck {
    Ok,
    Truncated { current_len: u64 },
    Rewritten,
}

fn append_to_seam_buf(seam_bytes: &mut Vec<u8>, committed_slice: &[u8]) {
    if committed_slice.is_empty() {
        return;
    }
    if committed_slice.len() >= SEAM_WINDOW_BYTES {
        seam_bytes.clear();
        seam_bytes.extend_from_slice(&committed_slice[committed_slice.len() - SEAM_WINDOW_BYTES..]);
    } else {
        let total = seam_bytes.len() + committed_slice.len();
        if total > SEAM_WINDOW_BYTES {
            let drop_count = total - SEAM_WINDOW_BYTES;
            seam_bytes.drain(..drop_count);
        }
        seam_bytes.extend_from_slice(committed_slice);
    }
}

fn read_seam_bytes(file: &File, end_offset: u64) -> Result<Vec<u8>, QuarryError> {
    if end_offset == 0 {
        return Ok(Vec::new());
    }
    let len = end_offset.min(SEAM_WINDOW_BYTES as u64) as usize;
    let start = end_offset - len as u64;
    let mut buf = vec![0u8; len];
    let read = read_at_exact_or_eof(file, start, &mut buf)?;
    if read < len {
        return Err(QuarryError::SourceChanged);
    }
    Ok(buf)
}

/// Inspects whether the last indexed row in `index` lacks a trailing newline, and if so returns
/// `(last_row_index, last_row_start_offset)`.
fn detect_unterminated_baseline_row(
    open_file: &File,
    file_size: u64,
    delimiter: u8,
    encoding: Encoding,
    index: &StructuralIndex,
) -> Result<Option<(u64, u64)>, QuarryError> {
    if file_size == 0 || index.indexed_rows() == 0 {
        return Ok(None);
    }
    let tail_width = encoding.width();
    if file_size < tail_width as u64 {
        return Ok(None);
    }
    let mut tail_unit = [0u8; 2];
    let read = read_at_exact_or_eof(
        open_file,
        file_size - tail_width as u64,
        &mut tail_unit[..tail_width],
    )?;
    if read < tail_width {
        return Err(QuarryError::SourceChanged);
    }
    if encoding.ends_with_ascii(&tail_unit[..tail_width], b'\n') {
        return Ok(None);
    }
    let last_row = index.indexed_rows() - 1;
    let rows = read_rows_from_open_file(
        open_file,
        delimiter,
        index,
        last_row,
        1,
        DEFAULT_MAX_RECORD_BYTES,
        encoding,
        file_size,
        true,
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some((last_row, row.offset)))
}

struct WatchShared {
    path: PathBuf,
    open_file: Arc<File>,
    open_identity: Option<FileIdentity>,
    delimiter: u8,
    encoding: Encoding,
    data_start: u64,
    max_record_bytes: usize,
    backend: WatchBackend,
    initial_committed_rows: u64,
    index: RwLock<StructuralIndex>,
    filter: RwLock<Option<WatchFilterState>>,
    snapshot: RwLock<WatchSnapshot>,
    stop_requested: AtomicBool,
    /// Used by the polling backend and for waking tests / shutdown cleanly.
    wake_cond: (Mutex<bool>, Condvar),
    #[cfg(any(
        target_os = "macos",
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    ))]
    wake_write_fd: Mutex<Option<OwnedRawFd>>,
}

impl WatchShared {
    fn signal_wakeup(&self) {
        {
            let (lock, cvar) = &self.wake_cond;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        #[cfg(any(
            target_os = "macos",
            all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            )
        ))]
        if let Some(fd) = self.wake_write_fd.lock().unwrap().as_ref() {
            write_wakeup_byte(fd);
        }
    }
}

/// Active opt-in append-only Live Watch job owned by a clean uncompressed document tab.
pub struct LiveWatchJob {
    dialect: Dialect,
    metrics: OpenMetrics,
    base_first_rows: Vec<Row>,
    cache_config: IndexCacheConfig,
    shared: Arc<WatchShared>,
    handle: Option<JoinHandle<WatchWorkerResult>>,
}

/// Background job that scans already-committed watched bytes for a new [`FilterQuery`] off the UI
/// thread and atomically hands off to incremental live evaluation with zero missed rows.
pub struct LiveWatchFilterJob {
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<Option<FilterIndex>, QuarryError>>>,
}

impl LiveWatchFilterJob {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    pub fn wait(mut self) -> Result<Option<FilterIndex>, QuarryError> {
        self.handle
            .take()
            .expect("live watch filter handle present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?
    }
}

impl Drop for LiveWatchFilterJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel();
            let _ = handle.join();
        }
    }
}

struct WatchWorkerResult {
    tail: TailScanner,
    final_status: WatchStatus,
}

impl LiveWatchJob {
    pub(crate) fn start(
        session: &Session,
        index: &StructuralIndex,
        active_filter: Option<&FilterIndex>,
        config: WatchConfig,
    ) -> Result<Self, QuarryError> {
        if session.is_compressed() {
            return Err(QuarryError::CompressedReadOnly);
        }
        if session.is_section() {
            return Err(QuarryError::InvalidOption(
                "Live Watch Mode is only available for whole files, not bounded sections.",
            ));
        }
        if !index.is_complete() {
            return Err(QuarryError::InvalidOption(
                "Wait for initial indexing to finish before starting Live Watch Mode.",
            ));
        }
        let (section_base, section_end) = session.active_byte_range();
        if !index.matches_session(
            &session.source_stamp,
            session.dialect.delimiter,
            session.encoding,
            section_base,
            section_end,
            false,
        ) {
            return Err(QuarryError::SourceChanged);
        }
        session.ensure_source_unchanged()?;

        let open_file = Arc::new(open_regular_file(&session.path)?);
        if !source_matches_stamp(&open_file, &session.path, &session.source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        let open_identity = session.source_stamp.file_identity();
        let file_size = session.file_size;
        let delimiter = session.dialect.delimiter;
        let encoding = session.encoding;
        let data_start = u64::from(session.dialect.has_header);

        if let Some(filter) = active_filter
            && !filter.is_complete_for_watch(
                &session.path,
                &session.source_stamp,
                delimiter,
                data_start,
                encoding,
            )
        {
            return Err(QuarryError::InvalidOption(
                "Active filter must be complete and match the current file before starting Live Watch Mode.",
            ));
        }

        let mut live_index = index.clone();
        let mut live_filter = active_filter.map(|f| WatchFilterState::new(f.clone(), data_start));

        // If the baseline file ended without a trailing newline, roll back that final row so it
        // remains provisional until the writer finishes and terminates it with a newline.
        let mut committed_record_end = file_size;
        let mut baseline_tail_bytes: Option<Vec<u8>> = None;
        if let Some((last_row, last_row_start)) = detect_unterminated_baseline_row(
            &open_file,
            file_size,
            delimiter,
            encoding,
            &live_index,
        )? {
            let tail_len =
                usize::try_from(file_size.saturating_sub(last_row_start)).map_err(|_| {
                    QuarryError::RecordTooLarge {
                        limit: config.max_record_bytes,
                    }
                })?;
            if tail_len > config.max_record_bytes {
                return Err(QuarryError::RecordTooLarge {
                    limit: config.max_record_bytes,
                });
            }
            let mut tail_buf = vec![0u8; tail_len];
            let read = read_at_exact_or_eof(&open_file, last_row_start, &mut tail_buf)?;
            if read < tail_len {
                return Err(QuarryError::SourceChanged);
            }

            // If an active filter matched the rolled-back unterminated row, roll back that single
            // match so it will be re-evaluated cleanly once terminated.
            if let Some(filter_state) = live_filter.as_mut()
                && last_row >= data_start
            {
                let query = filter_state.index.query().clone();
                let matchers = compile_matchers(&query)?;
                let groups = predicate_groups(&query);
                let mut field_ranges = Vec::new();
                if record_matches_fast(
                    &tail_buf,
                    delimiter,
                    last_row,
                    last_row_start == 0,
                    &query,
                    &matchers,
                    &groups,
                    filter_state.max_column,
                    &filter_state.edits,
                    encoding,
                    &mut field_ranges,
                )? {
                    filter_state.index.rollback_last_match_for_row(last_row);
                }
            }

            live_index.rollback_last_unterminated_row(last_row_start);
            committed_record_end = last_row_start;
            baseline_tail_bytes = Some(tail_buf);
        }

        let initial_committed_rows = live_index.indexed_rows();
        let mut tail = TailScanner::new(
            &open_file,
            committed_record_end,
            file_size,
            delimiter,
            encoding,
            config.max_record_bytes,
        )?;
        if let Some(tail_buf) = baseline_tail_bytes {
            tail.feed_bytes(&tail_buf, &mut live_index, live_filter.as_mut())?;
        }

        let network_fs = is_network_filesystem(&open_file);
        let poll_interval = if config.poll_interval.is_zero() {
            if network_fs {
                NETWORK_WATCH_POLL_INTERVAL
            } else {
                DEFAULT_WATCH_POLL_INTERVAL
            }
        } else if network_fs {
            config.poll_interval.max(NETWORK_WATCH_POLL_INTERVAL)
        } else {
            config.poll_interval
        };

        let (notifier, backend, wake_write_fd) = OsNotifier::create(
            &open_file,
            &session.path,
            config.force_polling || network_fs,
        );

        let initial_snapshot = WatchSnapshot {
            status: WatchStatus::Watching,
            backend,
            observed_file_len: tail.observed_file_len,
            scanner_consumed_bytes: tail.scanner_consumed_bytes,
            committed_record_end: tail.committed_record_end,
            committed_rows: live_index.indexed_rows(),
            appended_rows: 0,
            maximum_width: live_index.maximum_width(),
            provisional_tail: tail.provisional_tail(),
            filter_matches_found: live_filter.as_ref().map(|f| f.index.matches_found()),
            events_coalesced: 0,
            generation: 1,
        };

        let shared = Arc::new(WatchShared {
            path: session.path.clone(),
            open_file,
            open_identity,
            delimiter,
            encoding,
            data_start,
            max_record_bytes: config.max_record_bytes,
            backend,
            initial_committed_rows,
            index: RwLock::new(live_index),
            filter: RwLock::new(live_filter),
            snapshot: RwLock::new(initial_snapshot),
            stop_requested: AtomicBool::new(false),
            wake_cond: (Mutex::new(false), Condvar::new()),
            #[cfg(any(
                target_os = "macos",
                all(
                    target_os = "linux",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                )
            ))]
            wake_write_fd: Mutex::new(wake_write_fd),
        });
        #[cfg(not(any(
            target_os = "macos",
            all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            )
        )))]
        let _ = wake_write_fd;

        let worker_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("quarry-live-watch".into())
            .spawn(move || run_watch_loop(worker_shared, tail, notifier, poll_interval))?;

        Ok(Self {
            dialect: session.dialect,
            metrics: session.metrics,
            base_first_rows: session.first_rows.clone(),
            cache_config: config.cache_config,
            shared,
            handle: Some(handle),
        })
    }

    pub fn snapshot(&self) -> WatchSnapshot {
        self.shared.snapshot.read().unwrap().clone()
    }

    pub fn index_snapshot(&self) -> StructuralIndex {
        self.shared.index.read().unwrap().clone()
    }

    pub fn filter_index_snapshot(&self) -> Option<FilterIndex> {
        self.shared
            .filter
            .read()
            .unwrap()
            .as_ref()
            .map(|f| f.index.clone())
    }

    pub fn backend(&self) -> WatchBackend {
        self.shared.backend
    }

    /// Number of OS file descriptors currently held by this live watch job (for `FdBudgetLedger`).
    pub fn held_fd_count(&self) -> usize {
        // 1 pinned source FD + backend notifier/pipe FDs.
        1 + self.shared.backend.extra_fd_count()
    }

    /// Reads committed rows directly from the pinned open file descriptor, strictly bounded by
    /// `committed_record_end` (safe across concurrent appends and rename rotation).
    pub fn read_rows(&self, start: u64, count: usize) -> Result<Vec<Row>, QuarryError> {
        let index = self.shared.index.read().unwrap().clone();
        let committed_end = index.committed_record_end();
        read_rows_from_open_file(
            &self.shared.open_file,
            self.shared.delimiter,
            &index,
            start,
            count,
            self.shared.max_record_bytes,
            self.shared.encoding,
            committed_end,
            true,
        )
    }

    /// Reads filtered matches directly from the pinned open file descriptor, strictly bounded by
    /// `committed_record_end`.
    pub fn read_filtered_rows(
        &self,
        start_match: u64,
        count: usize,
    ) -> Result<Vec<FilterMatch>, QuarryError> {
        let committed_end = self.shared.index.read().unwrap().committed_record_end();
        let filter_index = self
            .shared
            .filter
            .read()
            .unwrap()
            .as_ref()
            .map(|f| f.index.clone())
            .ok_or(QuarryError::InvalidOption(
                "no active filter is attached to this Live Watch session",
            ))?;
        filter_index.read_matches_from_file(
            &self.shared.open_file,
            self.shared.delimiter,
            self.shared.encoding,
            committed_end,
            start_match,
            count,
            self.shared.max_record_bytes,
        )
    }

    /// Starts attaching a freshly evaluated filter index to the running watch off the UI thread,
    /// catching up to the latest committed frontier and atomically handing off to incremental
    /// evaluation with zero missed rows.
    pub fn start_attach_filter_query(
        &self,
        query: FilterQuery,
    ) -> Result<LiveWatchFilterJob, QuarryError> {
        query.validate()?;
        let shared = Arc::clone(&self.shared);
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_done = Arc::clone(&done);
        let handle = thread::Builder::new()
            .name("quarry-watch-filter".into())
            .spawn(move || {
                let result = attach_filter_query_shared(&shared, query, Some(&worker_cancel), None);
                worker_done.store(true, Ordering::Release);
                result
            })?;
        Ok(LiveWatchFilterJob {
            cancel,
            done,
            handle: Some(handle),
        })
    }

    /// Synchronously evaluates and atomically attaches a filter index to the running watch so all
    /// already-committed and concurrently appended rows are covered with zero missed matches.
    pub fn attach_filter_query(&self, query: FilterQuery) -> Result<FilterIndex, QuarryError> {
        attach_filter_query_shared(&self.shared, query, None, None)?.ok_or(
            QuarryError::InvalidOption("filter attachment was cancelled"),
        )
    }

    #[cfg(test)]
    fn attach_filter_query_with_hook(
        &self,
        query: FilterQuery,
        before_install: impl FnOnce() + Send + 'static,
    ) -> Result<FilterIndex, QuarryError> {
        attach_filter_query_shared(&self.shared, query, None, Some(Box::new(before_install)))?
            .ok_or(QuarryError::InvalidOption(
                "filter attachment was cancelled",
            ))
    }

    pub fn clear_filter(&self) {
        *self.shared.filter.write().unwrap() = None;
        let mut snap = self.shared.snapshot.write().unwrap();
        snap.filter_matches_found = None;
        snap.generation = snap.generation.saturating_add(1);
    }

    /// Triggers an immediate wakeup on the watch loop (useful in tests and manual checks).
    pub fn nudge(&self) {
        self.shared.signal_wakeup();
    }

    /// Stops Live Watch Mode, joins the watcher thread promptly, re-verifies seam + provisional
    /// bytes against the pinned descriptor, finalizes any valid trailing EOF record on clean stop,
    /// refreshes the persistent sidecar cache once via ordinary cache validation, and returns the
    /// reconciled [`Session`] and [`StructuralIndex`].
    pub fn stop(mut self) -> Result<WatchStopOutcome, QuarryError> {
        self.shared.stop_requested.store(true, Ordering::Release);
        self.shared.signal_wakeup();
        let worker_result = self
            .handle
            .take()
            .expect("live watch handle present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?;

        let mut tail = worker_result.tail;
        let mut final_status = worker_result.final_status;
        let mut index = self.shared.index.read().unwrap().clone();
        let mut filter_state = self.shared.filter.write().unwrap().take();

        // Bind final verification, provisional tail finalization, index provenance, and sidecar
        // publication to the exact same validated `SourceStamp`.
        let mut validated_stamp: Option<SourceStamp> = None;
        if matches!(final_status, WatchStatus::Stopped) {
            let mut read_buf = vec![0u8; WATCH_READ_CHUNK_BYTES];
            // Perform at most two quick bounded catch-up passes if bytes arrived right after the
            // worker exited so benign shutdown races still stop cleanly, while actively mutating
            // writers fail closed rather than returning a bogus `Stopped` with incomplete EOF.
            for _ in 0..2 {
                let meta_len = self.shared.open_file.metadata()?.len();
                if meta_len == tail.scanner_consumed_bytes {
                    break;
                }
                match drain_open_fd_locked(
                    &self.shared.open_file,
                    &mut tail,
                    &mut read_buf,
                    &mut index,
                    filter_state.as_mut(),
                ) {
                    Ok(DrainOutcome::Alert(alert)) => {
                        final_status = WatchStatus::Alert(alert);
                        break;
                    }
                    Ok(DrainOutcome::Changed | DrainOutcome::Unchanged) => {}
                    Err(err) => {
                        let alert = match err {
                            QuarryError::Io(io_err) => WatchAlert::IoError(io_err.to_string()),
                            other => WatchAlert::ParseError(other.to_string()),
                        };
                        final_status = WatchStatus::Alert(alert);
                        break;
                    }
                }
            }
        }

        if matches!(final_status, WatchStatus::Stopped) {
            let stamp_before = SourceStamp::from_file(&self.shared.open_file)?;
            if stamp_before.file_size() < tail.scanner_consumed_bytes {
                final_status = WatchStatus::Alert(WatchAlert::Truncated {
                    previous_len: tail.observed_file_len.max(tail.scanner_consumed_bytes),
                    current_len: stamp_before.file_size(),
                });
            } else if stamp_before.file_size() > tail.scanner_consumed_bytes {
                final_status = WatchStatus::Alert(WatchAlert::IoError(
                    "File continued growing while stopping Live Watch. Reload from disk to view the latest rows."
                        .into(),
                ));
            } else {
                match tail.verify_seam(&self.shared.open_file)? {
                    SeamCheck::Ok => {
                        let stamp_after = SourceStamp::from_file(&self.shared.open_file)?;
                        if stamp_after != stamp_before {
                            final_status = if stamp_after.file_size() < stamp_before.file_size() {
                                WatchStatus::Alert(WatchAlert::Truncated {
                                    previous_len: stamp_before.file_size(),
                                    current_len: stamp_after.file_size(),
                                })
                            } else {
                                WatchStatus::Alert(WatchAlert::Rewritten)
                            };
                        } else if let Err(err) =
                            tail.finish_on_clean_stop(&mut index, filter_state.as_mut())
                        {
                            final_status =
                                WatchStatus::Alert(WatchAlert::ParseError(err.to_string()));
                        } else {
                            validated_stamp = Some(stamp_after);
                        }
                    }
                    SeamCheck::Truncated { current_len } => {
                        final_status = WatchStatus::Alert(WatchAlert::Truncated {
                            previous_len: tail.observed_file_len.max(tail.committed_record_end),
                            current_len,
                        });
                    }
                    SeamCheck::Rewritten => {
                        final_status = WatchStatus::Alert(WatchAlert::Rewritten);
                    }
                }
            }
        }

        let final_stamp = match validated_stamp {
            Some(stamp) => stamp,
            None => SourceStamp::from_file(&self.shared.open_file)?,
        };
        let clean_eof = matches!(final_status, WatchStatus::Stopped)
            && index.committed_record_end() == final_stamp.file_size();
        index.set_complete(clean_eof);
        index.set_provenance(
            final_stamp.clone(),
            self.shared.delimiter,
            self.shared.encoding,
        );

        // On alert/incomplete stop, reuse the baseline `first_rows` captured at watch start so a
        // corrupted or malformed file at row 0 can never block alert delivery.
        let reuse_first_rows = (!clean_eof).then_some(self.base_first_rows.clone());
        let session = Session::from_watch_stop_parts(
            self.shared.path.clone(),
            self.shared.encoding,
            self.dialect,
            self.metrics,
            &self.shared.open_file,
            final_stamp.clone(),
            &index,
            reuse_first_rows,
        )?;

        let filter_index = filter_state.map(|mut state| {
            state.index.update_source_after_watch(
                self.shared.path.clone(),
                final_stamp.clone(),
                self.shared.delimiter,
                self.shared.data_start,
                self.shared.encoding,
                clean_eof,
            );
            state.index
        });

        // Never persist raw uncommitted tail bytes to sidecars; update sidecar at most once on
        // clean stop when the pinned descriptor still matches the exact validated `final_stamp`.
        if clean_eof
            && SourceStamp::from_file(&self.shared.open_file).ok().as_ref() == Some(&final_stamp)
            && source_matches_stamp(&self.shared.open_file, &self.shared.path, &final_stamp)
                .unwrap_or(false)
        {
            save_sidecar(
                &self.shared.open_file,
                &self.shared.path,
                &final_stamp,
                final_stamp.file_size(),
                self.shared.delimiter,
                self.shared.encoding,
                index.checkpoint_every(),
                &index,
                &self.cache_config,
            );
        }

        let final_snapshot = {
            let mut snap = self.shared.snapshot.write().unwrap();
            snap.status = final_status.clone();
            snap.observed_file_len = tail.observed_file_len;
            snap.scanner_consumed_bytes = tail.scanner_consumed_bytes;
            snap.committed_record_end = index.committed_record_end();
            snap.committed_rows = index.indexed_rows();
            snap.appended_rows = index
                .indexed_rows()
                .saturating_sub(self.shared.initial_committed_rows);
            snap.maximum_width = index.maximum_width();
            snap.provisional_tail = if clean_eof {
                None
            } else {
                tail.provisional_tail()
            };
            snap.filter_matches_found = filter_index.as_ref().map(FilterIndex::matches_found);
            snap.generation = snap.generation.saturating_add(1);
            snap.clone()
        };

        Ok(WatchStopOutcome {
            session,
            index,
            filter_index,
            final_status,
            snapshot: final_snapshot,
        })
    }
}

impl Drop for LiveWatchJob {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.shared.stop_requested.store(true, Ordering::Release);
            self.shared.signal_wakeup();
            let _ = handle.join();
        }
    }
}

fn attach_filter_query_shared(
    shared: &WatchShared,
    query: FilterQuery,
    cancel: Option<&AtomicBool>,
    mut before_install_hook: Option<Box<dyn FnOnce() + Send>>,
) -> Result<Option<FilterIndex>, QuarryError> {
    const INLINE_CATCHUP_MAX_BYTES: u64 = 256 * 1024;

    let stamp = SourceStamp::from_file(&shared.open_file)?;
    let mut filter_index = FilterIndex::new_for_watch(
        query.clone(),
        shared.path.clone(),
        stamp,
        shared.delimiter,
        shared.data_start,
        shared.encoding,
    )?;
    let matchers = compile_matchers(&query)?;
    let groups = predicate_groups(&query);
    let max_column = query
        .predicates
        .iter()
        .map(|predicate| predicate.column)
        .max()
        .unwrap_or(0);
    let edits: FilterEdits = Arc::default();
    let mut scanner = crate::encoding::RecordScanner::at_offset_with_bom_handling(
        shared.delimiter,
        0,
        shared.encoding,
        true,
    )?;
    let mut chunk = vec![0u8; shared.encoding.chunk_size(DEFAULT_READ_CHUNK)];
    let mut absolute_start = 0u64;
    let mut row_number = 0u64;
    let mut record_start = 0u64;
    let mut record = Vec::new();
    let mut field_ranges = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn scan_filter_range(
        shared: &WatchShared,
        query: &FilterQuery,
        matchers: &[crate::filter::PredicateMatcher<'_>],
        groups: &[Vec<usize>],
        max_column: usize,
        edits: &FilterEdits,
        scanner: &mut crate::encoding::RecordScanner,
        chunk: &mut [u8],
        absolute_start: &mut u64,
        target_end: u64,
        row_number: &mut u64,
        record_start: &mut u64,
        record: &mut Vec<u8>,
        field_ranges: &mut Vec<(usize, usize)>,
        filter_index: &mut FilterIndex,
        cancel: Option<&AtomicBool>,
    ) -> Result<bool, QuarryError> {
        while *absolute_start < target_end {
            if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Ok(false);
            }
            let remaining = target_end - *absolute_start;
            if shared.encoding.is_utf16() && remaining == 1 {
                return Err(QuarryError::SourceChanged);
            }
            let mut cap = usize::try_from(remaining)
                .unwrap_or(chunk.len())
                .min(chunk.len());
            if shared.encoding.is_utf16() {
                cap &= !1;
            }
            if cap == 0 {
                return Err(QuarryError::SourceChanged);
            }
            let read = read_at_exact_or_eof(&shared.open_file, *absolute_start, &mut chunk[..cap])?;
            if read < cap {
                // Short read below the known committed frontier indicates concurrent truncation.
                return Err(QuarryError::SourceChanged);
            }
            let mut seg_start = 0usize;
            let mut deferred_error = None;
            let start_offset = *absolute_start;
            scanner.scan_chunk(&chunk[..read], start_offset, |absolute_end| {
                let local_end = (absolute_end - start_offset) as usize;
                if *row_number >= shared.data_start && deferred_error.is_none() {
                    let rec_slice = if record.is_empty() {
                        &chunk[seg_start..local_end]
                    } else {
                        record.extend_from_slice(&chunk[seg_start..local_end]);
                        record.as_slice()
                    };
                    if rec_slice.len() > shared.max_record_bytes {
                        deferred_error = Some(QuarryError::RecordTooLarge {
                            limit: shared.max_record_bytes,
                        });
                    } else {
                        match record_matches_fast(
                            rec_slice,
                            shared.delimiter,
                            *row_number,
                            *record_start == 0,
                            query,
                            matchers,
                            groups,
                            max_column,
                            edits,
                            shared.encoding,
                            field_ranges,
                        ) {
                            Ok(true) => filter_index.record_match(*row_number, *record_start),
                            Ok(false) => {}
                            Err(err) => deferred_error = Some(err),
                        }
                    }
                }
                record.clear();
                *row_number += 1;
                *record_start = absolute_end;
                seg_start = local_end;
            })?;
            if let Some(err) = deferred_error {
                return Err(err);
            }
            if *row_number >= shared.data_start {
                record.extend_from_slice(&chunk[seg_start..read]);
                if record.len() > shared.max_record_bytes {
                    return Err(QuarryError::RecordTooLarge {
                        limit: shared.max_record_bytes,
                    });
                }
            }
            *absolute_start += read as u64;
        }
        Ok(true)
    }

    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Ok(None);
        }
        let target_end = shared.index.read().unwrap().committed_record_end();
        if !scan_filter_range(
            shared,
            &query,
            &matchers,
            &groups,
            max_column,
            &edits,
            &mut scanner,
            &mut chunk,
            &mut absolute_start,
            target_end,
            &mut row_number,
            &mut record_start,
            &mut record,
            &mut field_ranges,
            &mut filter_index,
            cancel,
        )? {
            return Ok(None);
        }
        if let Some(hook) = before_install_hook.take() {
            hook();
        }
        // Acquire locks in the exact same order as `reconcile_once` (`index` then `filter`) so
        // no concurrent commit can land between our frontier check and filter installation.
        let index_guard = shared.index.read().unwrap();
        let mut filter_guard = shared.filter.write().unwrap();
        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Ok(None);
        }
        let latest_end = index_guard.committed_record_end();
        if latest_end < absolute_start {
            return Err(QuarryError::SourceChanged);
        }
        let gap = latest_end - absolute_start;
        if gap > INLINE_CATCHUP_MAX_BYTES {
            drop(filter_guard);
            drop(index_guard);
            continue;
        }
        if gap > 0
            && !scan_filter_range(
                shared,
                &query,
                &matchers,
                &groups,
                max_column,
                &edits,
                &mut scanner,
                &mut chunk,
                &mut absolute_start,
                latest_end,
                &mut row_number,
                &mut record_start,
                &mut record,
                &mut field_ranges,
                &mut filter_index,
                cancel,
            )?
        {
            return Ok(None);
        }
        filter_index.set_complete(true);
        let snapshot_index = filter_index.clone();
        *filter_guard = Some(WatchFilterState::new(filter_index, shared.data_start));
        drop(filter_guard);
        drop(index_guard);
        {
            let mut snap = shared.snapshot.write().unwrap();
            snap.filter_matches_found = Some(snapshot_index.matches_found());
            snap.generation = snap.generation.saturating_add(1);
        }
        return Ok(Some(snapshot_index));
    }
}

fn run_watch_loop(
    shared: Arc<WatchShared>,
    mut tail: TailScanner,
    mut notifier: OsNotifier,
    poll_interval: Duration,
) -> WatchWorkerResult {
    let mut read_buf = vec![0u8; WATCH_READ_CHUNK_BYTES];
    let mut status = WatchStatus::Watching;
    let mut events_coalesced = 0u64;

    loop {
        if shared.stop_requested.load(Ordering::Acquire) {
            // Drain any already-written bytes once before clean stop when still actively watching,
            // propagating both alerts and scan/IO errors rather than reporting a false `Stopped`.
            if status.is_active() {
                match reconcile_once(&shared, &mut tail, &mut read_buf, &mut status) {
                    Ok(ReconcileOutcome::Alert(alert)) => {
                        status = WatchStatus::Alert(alert);
                        publish_snapshot(&shared, &tail, &status, events_coalesced);
                        return WatchWorkerResult {
                            tail,
                            final_status: status,
                        };
                    }
                    Err(err) => {
                        let alert = match err {
                            QuarryError::Io(io_err) => WatchAlert::IoError(io_err.to_string()),
                            other => WatchAlert::ParseError(other.to_string()),
                        };
                        status = WatchStatus::Alert(alert);
                        publish_snapshot(&shared, &tail, &status, events_coalesced);
                        return WatchWorkerResult {
                            tail,
                            final_status: status,
                        };
                    }
                    Ok(ReconcileOutcome::Changed | ReconcileOutcome::Unchanged) => {}
                }
            }
            status = WatchStatus::Stopped;
            publish_snapshot(&shared, &tail, &status, events_coalesced);
            return WatchWorkerResult {
                tail,
                final_status: status,
            };
        }

        let coalesced = notifier.wait_for_events(&shared, poll_interval);
        events_coalesced = events_coalesced.saturating_add(coalesced);

        if shared.stop_requested.load(Ordering::Acquire) {
            continue;
        }

        match reconcile_once(&shared, &mut tail, &mut read_buf, &mut status) {
            Ok(ReconcileOutcome::Changed) => {
                publish_snapshot(&shared, &tail, &status, events_coalesced);
            }
            Ok(ReconcileOutcome::Unchanged) => {
                if coalesced > 0 {
                    let mut snap = shared.snapshot.write().unwrap();
                    snap.events_coalesced = events_coalesced;
                }
            }
            Ok(ReconcileOutcome::Alert(alert)) => {
                status = WatchStatus::Alert(alert);
                publish_snapshot(&shared, &tail, &status, events_coalesced);
                return WatchWorkerResult {
                    tail,
                    final_status: status,
                };
            }
            Err(err) => {
                let alert = match err {
                    QuarryError::Io(io_err) => WatchAlert::IoError(io_err.to_string()),
                    other => WatchAlert::ParseError(other.to_string()),
                };
                status = WatchStatus::Alert(alert);
                publish_snapshot(&shared, &tail, &status, events_coalesced);
                return WatchWorkerResult {
                    tail,
                    final_status: status,
                };
            }
        }
    }
}

enum ReconcileOutcome {
    Unchanged,
    Changed,
    Alert(WatchAlert),
}

enum PathIdentityCheck {
    SameFile,
    Missing,
    Replaced,
}

fn check_path_identity(path: &Path, expected: Option<FileIdentity>) -> PathIdentityCheck {
    let Some(expected_id) = expected else {
        return PathIdentityCheck::SameFile;
    };
    match open_regular_file(path) {
        Ok(file) => match SourceStamp::from_file(&file) {
            Ok(stamp) if stamp.file_identity() == Some(expected_id) => PathIdentityCheck::SameFile,
            Ok(_) => PathIdentityCheck::Replaced,
            Err(_) => PathIdentityCheck::Missing,
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => PathIdentityCheck::Missing,
        Err(_) => PathIdentityCheck::Replaced,
    }
}

enum DrainOutcome {
    Unchanged,
    Changed,
    Alert(WatchAlert),
}

fn drain_open_fd_locked(
    open_file: &File,
    tail: &mut TailScanner,
    read_buf: &mut [u8],
    index: &mut StructuralIndex,
    mut filter: Option<&mut WatchFilterState>,
) -> Result<DrainOutcome, QuarryError> {
    let mut changed = false;

    let meta = open_file.metadata()?;
    let current_len = meta.len();
    if current_len < tail.observed_file_len || current_len < tail.committed_record_end {
        let previous_len = tail.observed_file_len.max(tail.committed_record_end);
        tail.observed_file_len = current_len;
        return Ok(DrainOutcome::Alert(WatchAlert::Truncated {
            previous_len,
            current_len,
        }));
    }

    match tail.verify_seam(open_file)? {
        SeamCheck::Ok => {}
        SeamCheck::Truncated { current_len } => {
            let previous_len = tail.observed_file_len.max(tail.committed_record_end);
            tail.observed_file_len = current_len;
            return Ok(DrainOutcome::Alert(WatchAlert::Truncated {
                previous_len,
                current_len,
            }));
        }
        SeamCheck::Rewritten => {
            return Ok(DrainOutcome::Alert(WatchAlert::Rewritten));
        }
    }

    if current_len > tail.observed_file_len {
        tail.observed_file_len = current_len;
        changed = true;
    }

    while tail.scanner_consumed_bytes < tail.observed_file_len {
        let want = usize::try_from(tail.observed_file_len - tail.scanner_consumed_bytes)
            .unwrap_or(read_buf.len())
            .min(read_buf.len());
        let read = read_at_exact_or_eof(
            open_file,
            tail.scanner_consumed_bytes,
            &mut read_buf[..want],
        )?;
        if read < want {
            let previous_len = tail.observed_file_len;
            let actual_len = tail.scanner_consumed_bytes + read as u64;
            tail.observed_file_len = actual_len;
            return Ok(DrainOutcome::Alert(WatchAlert::Truncated {
                previous_len,
                current_len: actual_len,
            }));
        }

        let committed = tail.feed_bytes(&read_buf[..read], index, filter.as_deref_mut())?;
        if committed > 0 {
            changed = true;
        }
    }

    Ok(if changed {
        DrainOutcome::Changed
    } else {
        DrainOutcome::Unchanged
    })
}

fn drain_open_fd(
    shared: &WatchShared,
    tail: &mut TailScanner,
    read_buf: &mut [u8],
) -> Result<DrainOutcome, QuarryError> {
    let mut changed = false;

    // 1. Reconcile open FD size and catch truncation before reading.
    let meta = shared.open_file.metadata()?;
    let current_len = meta.len();
    if current_len < tail.observed_file_len || current_len < tail.committed_record_end {
        let previous_len = tail.observed_file_len.max(tail.committed_record_end);
        tail.observed_file_len = current_len;
        return Ok(DrainOutcome::Alert(WatchAlert::Truncated {
            previous_len,
            current_len,
        }));
    }

    // 2. Verify committed tail seam + retained provisional bytes on every reconciliation check.
    match tail.verify_seam(&shared.open_file)? {
        SeamCheck::Ok => {}
        SeamCheck::Truncated { current_len } => {
            let previous_len = tail.observed_file_len.max(tail.committed_record_end);
            tail.observed_file_len = current_len;
            return Ok(DrainOutcome::Alert(WatchAlert::Truncated {
                previous_len,
                current_len,
            }));
        }
        SeamCheck::Rewritten => {
            return Ok(DrainOutcome::Alert(WatchAlert::Rewritten));
        }
    }

    // 3. Drain all newly appended bytes from the pinned open FD (`[scanner_consumed_bytes, current_len)`).
    if current_len > tail.observed_file_len {
        tail.observed_file_len = current_len;
        changed = true;
    }

    while tail.scanner_consumed_bytes < tail.observed_file_len {
        let want = usize::try_from(tail.observed_file_len - tail.scanner_consumed_bytes)
            .unwrap_or(read_buf.len())
            .min(read_buf.len());
        let read = read_at_exact_or_eof(
            &shared.open_file,
            tail.scanner_consumed_bytes,
            &mut read_buf[..want],
        )?;
        if read < want {
            // Short read on an expected grown region means concurrent truncation.
            let previous_len = tail.observed_file_len;
            let actual_len = tail.scanner_consumed_bytes + read as u64;
            tail.observed_file_len = actual_len;
            return Ok(DrainOutcome::Alert(WatchAlert::Truncated {
                previous_len,
                current_len: actual_len,
            }));
        }

        let mut index_guard = shared.index.write().unwrap();
        let mut filter_guard = shared.filter.write().unwrap();
        let committed =
            tail.feed_bytes(&read_buf[..read], &mut index_guard, filter_guard.as_mut())?;
        if committed > 0 {
            changed = true;
        }
    }

    Ok(if changed {
        DrainOutcome::Changed
    } else {
        DrainOutcome::Unchanged
    })
}

fn reconcile_once(
    shared: &WatchShared,
    tail: &mut TailScanner,
    read_buf: &mut [u8],
    status: &mut WatchStatus,
) -> Result<ReconcileOutcome, QuarryError> {
    let mut changed = match drain_open_fd(shared, tail, read_buf)? {
        DrainOutcome::Alert(alert) => return Ok(ReconcileOutcome::Alert(alert)),
        DrainOutcome::Changed => true,
        DrainOutcome::Unchanged => false,
    };

    // 4. Reconcile current path identity (draining the open FD again on `Replaced` so any final
    //    records written to the old inode between step 3 and replacement are delivered before
    //    stopping and offering "Open new file").
    match check_path_identity(&shared.path, shared.open_identity) {
        PathIdentityCheck::SameFile => {
            if *status == WatchStatus::FollowingUnlinked {
                *status = WatchStatus::Watching;
                changed = true;
            }
        }
        PathIdentityCheck::Missing => {
            if *status != WatchStatus::FollowingUnlinked {
                *status = WatchStatus::FollowingUnlinked;
                changed = true;
            }
        }
        PathIdentityCheck::Replaced => {
            if let DrainOutcome::Alert(alert) = drain_open_fd(shared, tail, read_buf)? {
                return Ok(ReconcileOutcome::Alert(alert));
            }
            return Ok(ReconcileOutcome::Alert(WatchAlert::Rotated));
        }
    }

    Ok(if changed {
        ReconcileOutcome::Changed
    } else {
        ReconcileOutcome::Unchanged
    })
}

fn publish_snapshot(
    shared: &WatchShared,
    tail: &TailScanner,
    status: &WatchStatus,
    events_coalesced: u64,
) {
    let (committed_rows, maximum_width) = {
        let index = shared.index.read().unwrap();
        (index.indexed_rows(), index.maximum_width())
    };
    let filter_matches_found = shared
        .filter
        .read()
        .unwrap()
        .as_ref()
        .map(|f| f.index.matches_found());
    let mut snap = shared.snapshot.write().unwrap();
    snap.status = status.clone();
    snap.observed_file_len = tail.observed_file_len;
    snap.scanner_consumed_bytes = tail.scanner_consumed_bytes;
    snap.committed_record_end = tail.committed_record_end;
    snap.committed_rows = committed_rows;
    snap.appended_rows = committed_rows.saturating_sub(shared.initial_committed_rows);
    snap.maximum_width = maximum_width;
    snap.provisional_tail = tail.provisional_tail();
    snap.filter_matches_found = filter_matches_found;
    snap.events_coalesced = events_coalesced;
    snap.generation = snap.generation.saturating_add(1);
}

// ---------------------------------------------------------------------------
// Platform-native OS notification backends (`kqueue`, `inotify`, `poll`)
// ---------------------------------------------------------------------------

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod linux_sys {
    use std::ffi::{c_char, c_int, c_short, c_void};

    // Linux asm-generic constants shared identically by x86_64 and aarch64.
    pub const O_NONBLOCK: c_int = 0o0004000;
    pub const O_CLOEXEC: c_int = 0o2000000;
    pub const IN_NONBLOCK: c_int = O_NONBLOCK;
    pub const IN_CLOEXEC: c_int = O_CLOEXEC;
    pub const IN_MODIFY: u32 = 0x0000_0002;
    pub const IN_ATTRIB: u32 = 0x0000_0004;
    pub const IN_MOVED_FROM: u32 = 0x0000_0040;
    pub const IN_MOVED_TO: u32 = 0x0000_0080;
    pub const IN_CREATE: u32 = 0x0000_0100;
    pub const IN_DELETE: u32 = 0x0000_0200;
    pub const IN_DELETE_SELF: u32 = 0x0000_0400;
    pub const IN_MOVE_SELF: u32 = 0x0000_0800;
    pub const POLLIN: c_short = 0x0001;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PollFd {
        pub fd: c_int,
        pub events: c_short,
        pub revents: c_short,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct InotifyEvent {
        pub wd: c_int,
        pub mask: u32,
        pub cookie: u32,
        pub len: u32,
    }

    unsafe extern "C" {
        pub fn pipe2(fds: *mut c_int, flags: c_int) -> c_int;
        pub fn inotify_init1(flags: c_int) -> c_int;
        pub fn inotify_add_watch(fd: c_int, pathname: *const c_char, mask: u32) -> c_int;
        pub fn poll(fds: *mut PollFd, nfds: usize, timeout: c_int) -> c_int;
        pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        pub fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
        pub fn close(fd: c_int) -> c_int;
    }
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
struct OwnedRawFd(std::ffi::c_int);

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
impl OwnedRawFd {
    fn raw(&self) -> std::ffi::c_int {
        self.0
    }
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
impl Drop for OwnedRawFd {
    fn drop(&mut self) {
        // SAFETY: `self.0` is an exclusively owned valid file descriptor.
        unsafe {
            #[cfg(target_os = "macos")]
            libc::close(self.0);
            #[cfg(target_os = "linux")]
            linux_sys::close(self.0);
        }
    }
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
fn write_wakeup_byte(write_fd: &OwnedRawFd) {
    let byte = [1u8];
    // SAFETY: writing one byte to our non-blocking self-pipe only wakes poll/kevent.
    unsafe {
        #[cfg(target_os = "macos")]
        let _ = libc::write(write_fd.raw(), byte.as_ptr().cast(), 1);
        #[cfg(target_os = "linux")]
        let _ = linux_sys::write(write_fd.raw(), byte.as_ptr().cast(), 1);
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn create_wakeup_pipe() -> io::Result<(OwnedRawFd, OwnedRawFd)> {
    let mut fds = [0 as std::ffi::c_int; 2];
    // SAFETY: `fds` is a valid 2-element array for `pipe2`.
    if unsafe {
        linux_sys::pipe2(
            fds.as_mut_ptr(),
            linux_sys::O_NONBLOCK | linux_sys::O_CLOEXEC,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((OwnedRawFd(fds[0]), OwnedRawFd(fds[1])))
}

#[cfg(target_os = "macos")]
fn create_wakeup_pipe() -> io::Result<(OwnedRawFd, OwnedRawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid 2-element array for `pipe`.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let read_fd = OwnedRawFd(fds[0]);
    let write_fd = OwnedRawFd(fds[1]);
    for fd in [&read_fd, &write_fd] {
        // SAFETY: setting `FD_CLOEXEC` and `O_NONBLOCK` on our freshly opened pipe descriptors.
        let ok = unsafe {
            let flags = libc::fcntl(fd.raw(), libc::F_GETFD);
            let cloexec_set =
                flags >= 0 && libc::fcntl(fd.raw(), libc::F_SETFD, flags | libc::FD_CLOEXEC) >= 0;
            let fl = if cloexec_set {
                libc::fcntl(fd.raw(), libc::F_GETFL)
            } else {
                -1
            };
            fl >= 0 && libc::fcntl(fd.raw(), libc::F_SETFL, fl | libc::O_NONBLOCK) >= 0
        };
        if !ok {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read_fd, write_fd))
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
fn drain_wakeup_pipe(read_fd: &OwnedRawFd) {
    let mut buf = [0u8; 64];
    loop {
        // SAFETY: non-blocking read into stack buffer from owned pipe FD.
        let n = unsafe {
            #[cfg(target_os = "macos")]
            {
                libc::read(read_fd.raw(), buf.as_mut_ptr().cast(), buf.len())
            }
            #[cfg(target_os = "linux")]
            {
                linux_sys::read(read_fd.raw(), buf.as_mut_ptr().cast(), buf.len())
            }
        };
        if n <= 0 {
            break;
        }
    }
}

enum OsNotifier {
    #[cfg(target_os = "macos")]
    Kqueue {
        kq: OwnedRawFd,
        wake_read: OwnedRawFd,
    },
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    Inotify {
        inotify_fd: OwnedRawFd,
        wake_read: OwnedRawFd,
    },
    Polling,
}

impl OsNotifier {
    #[cfg(target_os = "macos")]
    fn create(
        open_file: &File,
        _path: &Path,
        use_polling: bool,
    ) -> (Self, WatchBackend, Option<OwnedRawFd>) {
        use std::os::fd::AsRawFd;

        if use_polling {
            return (Self::Polling, WatchBackend::Polling, None);
        }
        let Ok((wake_read, wake_write)) = create_wakeup_pipe() else {
            return (Self::Polling, WatchBackend::Polling, None);
        };
        // SAFETY: `kqueue()` takes no pointers and returns a new FD or -1.
        let raw_kq = unsafe { libc::kqueue() };
        if raw_kq < 0 {
            return (Self::Polling, WatchBackend::Polling, None);
        }
        let kq = OwnedRawFd(raw_kq);
        // SAFETY: setting `FD_CLOEXEC` on our newly opened kqueue FD.
        let cloexec_ok = unsafe {
            let flags = libc::fcntl(kq.raw(), libc::F_GETFD);
            flags >= 0 && libc::fcntl(kq.raw(), libc::F_SETFD, flags | libc::FD_CLOEXEC) >= 0
        };
        if !cloexec_ok {
            return (Self::Polling, WatchBackend::Polling, None);
        }
        let changelist = [
            libc::kevent {
                ident: open_file.as_raw_fd() as usize,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: libc::NOTE_EXTEND
                    | libc::NOTE_WRITE
                    | libc::NOTE_DELETE
                    | libc::NOTE_RENAME
                    | libc::NOTE_ATTRIB
                    | libc::NOTE_REVOKE,
                data: 0,
                udata: std::ptr::null_mut(),
            },
            libc::kevent {
                ident: wake_read.raw() as usize,
                filter: libc::EVFILT_READ,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            },
        ];
        // SAFETY: registering two valid `kevent` entries on our owned kqueue FD.
        let rc = unsafe {
            libc::kevent(
                kq.raw(),
                changelist.as_ptr(),
                changelist.len() as libc::c_int,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if rc < 0 {
            return (Self::Polling, WatchBackend::Polling, None);
        }
        (
            Self::Kqueue { kq, wake_read },
            WatchBackend::Kqueue,
            Some(wake_write),
        )
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    fn create(
        _open_file: &File,
        path: &Path,
        use_polling: bool,
    ) -> (Self, WatchBackend, Option<OwnedRawFd>) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        if use_polling {
            return (Self::Polling, WatchBackend::Polling, None);
        }
        let Ok((wake_read, wake_write)) = create_wakeup_pipe() else {
            return (Self::Polling, WatchBackend::Polling, None);
        };
        // SAFETY: `inotify_init1` returns a new FD or -1.
        let raw_fd =
            unsafe { linux_sys::inotify_init1(linux_sys::IN_NONBLOCK | linux_sys::IN_CLOEXEC) };
        if raw_fd < 0 {
            return (Self::Polling, WatchBackend::Polling, None);
        }
        let inotify_fd = OwnedRawFd(raw_fd);
        let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
            return (Self::Polling, WatchBackend::Polling, None);
        };
        // SAFETY: `c_path` is NUL-terminated and `inotify_fd` is owned.
        let file_wd = unsafe {
            linux_sys::inotify_add_watch(
                inotify_fd.raw(),
                c_path.as_ptr(),
                linux_sys::IN_MODIFY
                    | linux_sys::IN_ATTRIB
                    | linux_sys::IN_DELETE_SELF
                    | linux_sys::IN_MOVE_SELF,
            )
        };
        if file_wd < 0 {
            return (Self::Polling, WatchBackend::Polling, None);
        }
        if let Some(parent) = path.parent()
            && let Ok(c_parent) = CString::new(parent.as_os_str().as_bytes())
        {
            // Best-effort parent directory watch so rename-rotation wakes promptly.
            // SAFETY: `c_parent` is NUL-terminated and `inotify_fd` is owned.
            let _ = unsafe {
                linux_sys::inotify_add_watch(
                    inotify_fd.raw(),
                    c_parent.as_ptr(),
                    linux_sys::IN_MOVED_TO
                        | linux_sys::IN_CREATE
                        | linux_sys::IN_MOVED_FROM
                        | linux_sys::IN_DELETE,
                )
            };
        }
        (
            Self::Inotify {
                inotify_fd,
                wake_read,
            },
            WatchBackend::Inotify,
            Some(wake_write),
        )
    }

    #[cfg(not(any(
        target_os = "macos",
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    )))]
    fn create(
        _open_file: &File,
        _path: &Path,
        _use_polling: bool,
    ) -> (Self, WatchBackend, Option<()>) {
        (Self::Polling, WatchBackend::Polling, None)
    }

    /// Waits up to `timeout` for OS file events or a shutdown wakeup, draining and returning the
    /// number of coalesced OS events observed.
    fn wait_for_events(&mut self, shared: &WatchShared, timeout: Duration) -> u64 {
        match self {
            #[cfg(target_os = "macos")]
            Self::Kqueue { kq, wake_read } => {
                let ts = libc::timespec {
                    tv_sec: timeout.as_secs() as libc::time_t,
                    tv_nsec: timeout.subsec_nanos() as libc::c_long,
                };
                let mut events = [libc::kevent {
                    ident: 0,
                    filter: 0,
                    flags: 0,
                    fflags: 0,
                    data: 0,
                    udata: std::ptr::null_mut(),
                }; 8];
                // SAFETY: `kq` is owned and `events` + `ts` are valid stack structs.
                let n = unsafe {
                    libc::kevent(
                        kq.raw(),
                        std::ptr::null(),
                        0,
                        events.as_mut_ptr(),
                        events.len() as libc::c_int,
                        &ts,
                    )
                };
                if n <= 0 {
                    return 0;
                }
                let mut count = 0u64;
                for event in &events[..(n as usize)] {
                    if event.ident == wake_read.raw() as usize {
                        drain_wakeup_pipe(wake_read);
                    } else {
                        count += 1;
                    }
                }
                count
            }
            #[cfg(all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ))]
            Self::Inotify {
                inotify_fd,
                wake_read,
            } => {
                let mut fds = [
                    linux_sys::PollFd {
                        fd: inotify_fd.raw(),
                        events: linux_sys::POLLIN,
                        revents: 0,
                    },
                    linux_sys::PollFd {
                        fd: wake_read.raw(),
                        events: linux_sys::POLLIN,
                        revents: 0,
                    },
                ];
                let timeout_ms =
                    timeout.as_millis().min(std::ffi::c_int::MAX as u128) as std::ffi::c_int;
                // SAFETY: `fds` holds two valid owned file descriptors.
                let rc = unsafe { linux_sys::poll(fds.as_mut_ptr(), fds.len(), timeout_ms) };
                if rc <= 0 {
                    return 0;
                }
                if (fds[1].revents & linux_sys::POLLIN) != 0 {
                    drain_wakeup_pipe(wake_read);
                }
                if (fds[0].revents & linux_sys::POLLIN) == 0 {
                    return 0;
                }
                // Drain all queued inotify events to coalesce bursts into one reconciliation pass.
                let mut buf = [0u8; 4096];
                let mut events_count = 0u64;
                loop {
                    // SAFETY: non-blocking read from owned inotify descriptor.
                    let n = unsafe {
                        linux_sys::read(inotify_fd.raw(), buf.as_mut_ptr().cast(), buf.len())
                    };
                    if n <= 0 {
                        break;
                    }
                    let mut pos = 0usize;
                    let len = n as usize;
                    let hdr_size = std::mem::size_of::<linux_sys::InotifyEvent>();
                    while pos + hdr_size <= len {
                        // SAFETY: reading an aligned/unaligned `InotifyEvent` header inside `buf[..len]`.
                        let ev = unsafe {
                            std::ptr::read_unaligned(
                                buf.as_ptr().add(pos).cast::<linux_sys::InotifyEvent>(),
                            )
                        };
                        events_count += 1;
                        let step = hdr_size.saturating_add(ev.len as usize);
                        if step == 0 {
                            break;
                        }
                        pos = pos.saturating_add(step);
                    }
                }
                events_count
            }
            Self::Polling => {
                let (lock, cvar) = &shared.wake_cond;
                let mut woken = lock.lock().unwrap();
                if !*woken && !shared.stop_requested.load(Ordering::Acquire) {
                    let (guard, _) = cvar.wait_timeout(woken, timeout).unwrap();
                    woken = guard;
                }
                let was_woken = *woken;
                *woken = false;
                u64::from(was_woken)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FilterOperator, HeaderMode, IndexConfig, OpenOptions};
    use std::fs::OpenOptions as FsOpenOptions;
    use std::io::Write;
    use std::time::Instant;

    fn wait_until<F>(timeout: Duration, mut pred: F) -> bool
    where
        F: FnMut() -> bool,
    {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pred() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        pred()
    }

    fn open_and_index(
        path: &Path,
        encoding: Option<Encoding>,
        header_mode: HeaderMode,
    ) -> (Session, StructuralIndex) {
        let session = Session::open_with_encoding(
            path,
            OpenOptions {
                header_mode,
                ..Default::default()
            },
            encoding,
        )
        .unwrap();
        let index = session
            .start_indexing_with_options(
                IndexConfig::default(),
                Some(1),
                IndexCacheConfig::disabled(),
            )
            .unwrap()
            .wait()
            .unwrap();
        (session, index)
    }

    #[test]
    fn provisional_tail_and_split_utf8_crlf_quotes_match_independent_oracle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("split_utf8.csv");
        std::fs::write(&path, b"id,note\r\n1,init\r\n").unwrap();

        let (session, index) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let watch = session
            .start_live_watch(
                &index,
                None,
                WatchConfig {
                    poll_interval: Duration::from_millis(20),
                    cache_config: IndexCacheConfig::disabled(),
                    ..Default::default()
                },
            )
            .unwrap();

        let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
        // Write 1: unquoted partial row with split 3-byte UTF-8 '€' (E2 82 AC) -> must stay provisional
        writer.write_all(b"2,cost \xE2").unwrap();
        writer.flush().unwrap();
        watch.nudge();
        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().observed_file_len == 25
        }));
        let snap1 = watch.snapshot();
        assert_eq!(snap1.committed_rows, 2);
        assert_eq!(snap1.committed_record_end, 17);
        assert_eq!(snap1.scanner_consumed_bytes, 25);
        assert_eq!(snap1.provisional_tail.as_ref().unwrap().byte_len, 8);
        assert!(
            snap1
                .provisional_tail
                .as_ref()
                .unwrap()
                .preview_fields
                .is_none()
        );

        // Write 2: finish UTF-8 '€' + trailing '\r' (CRLF split across writes!)
        writer.write_all(b"\x82\xAC\r").unwrap();
        writer.flush().unwrap();
        watch.nudge();
        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().observed_file_len == 28
        }));
        assert_eq!(watch.snapshot().committed_rows, 2);

        // Write 3: finish LF + start multiline quoted field with split doubled quote ""
        writer.write_all(b"\n3,\"hello\r\n\"\"").unwrap();
        writer.flush().unwrap();
        watch.nudge();
        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().committed_rows == 3 && watch.snapshot().observed_file_len == 41
        }));
        assert_eq!(watch.snapshot().committed_rows, 3);
        let row2 = watch.read_rows(2, 1).unwrap();
        assert_eq!(row2[0].fields[1], "cost €".as_bytes());

        // Write 4: finish quoted field with split closing quote + CR, then LF in write 5
        writer.write_all(b"world\"\r").unwrap();
        writer.flush().unwrap();
        watch.nudge();
        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().observed_file_len == 48
        }));
        assert_eq!(watch.snapshot().committed_rows, 3);

        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
        watch.nudge();
        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().committed_rows == 4
        }));

        let row3 = watch.read_rows(3, 1).unwrap();
        assert_eq!(row3[0].fields[0], b"3");
        assert_eq!(row3[0].fields[1], b"hello\r\n\"world");

        let stopped = watch.stop().unwrap();
        assert_eq!(stopped.final_status, WatchStatus::Stopped);

        // Validate 100% parity against an independent fresh static open + index.
        let (oracle_session, oracle_index) =
            open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        assert_eq!(stopped.index.indexed_rows(), oracle_index.indexed_rows());
        assert_eq!(
            stopped.session.read_rows(&stopped.index, 0, 10).unwrap(),
            oracle_session.read_rows(&oracle_index, 0, 10).unwrap()
        );
    }

    #[test]
    fn utf16_odd_byte_and_split_surrogate_carry_cleanly() {
        for encoding in [Encoding::Utf16Le, Encoding::Utf16Be] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("split_{encoding}.csv"));
            let mut initial = encoding.bom().to_vec();
            initial.extend_from_slice(&encoding.encode(b"k,v\n", 1024).unwrap());
            std::fs::write(&path, &initial).unwrap();

            let (session, index) = open_and_index(&path, Some(encoding), HeaderMode::FirstRow);
            let watch = session
                .start_live_watch(
                    &index,
                    None,
                    WatchConfig {
                        poll_interval: Duration::from_millis(15),
                        cache_config: IndexCacheConfig::disabled(),
                        ..Default::default()
                    },
                )
                .unwrap();

            // Encode "1,😀\r\n" and drip it 1 byte at a time (hits odd UTF-16 byte + split surrogate + split CRLF!).
            let appended = encoding.encode("1,😀\r\n2,ok\n".as_bytes(), 1024).unwrap();
            let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
            for &byte in appended.as_ref() {
                writer.write_all(&[byte]).unwrap();
                writer.flush().unwrap();
                watch.nudge();
                let target_len = writer.metadata().unwrap().len();
                assert!(wait_until(Duration::from_secs(2), || {
                    watch.snapshot().observed_file_len == target_len
                }));
                assert_eq!(watch.snapshot().status, WatchStatus::Watching);
            }
            assert!(wait_until(Duration::from_secs(2), || {
                watch.snapshot().committed_rows == 3
            }));

            let rows = watch.read_rows(1, 2).unwrap();
            assert_eq!(rows[0].fields[1], "😀".as_bytes());
            assert_eq!(rows[1].fields[1], b"ok");

            let stopped = watch.stop().unwrap();
            assert_eq!(stopped.final_status, WatchStatus::Stopped);
            let (oracle_session, oracle_index) =
                open_and_index(&path, Some(encoding), HeaderMode::FirstRow);
            assert_eq!(
                stopped.session.read_rows(&stopped.index, 0, 10).unwrap(),
                oracle_session.read_rows(&oracle_index, 0, 10).unwrap()
            );
        }
    }

    #[test]
    fn unterminated_baseline_row_rolls_back_and_extends_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unterminated.csv");
        // No trailing newline on row 1!
        std::fs::write(&path, b"id,val\n1,par").unwrap();

        let (session, index) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        assert_eq!(index.indexed_rows(), 2);

        let query = FilterQuery::single(1, FilterOperator::Equals, b"partial".to_vec());
        let initial_filter = session.start_filter(query).unwrap().wait().unwrap();
        assert_eq!(initial_filter.matches_found(), 0);

        let watch = session
            .start_live_watch(
                &index,
                Some(&initial_filter),
                WatchConfig {
                    poll_interval: Duration::from_millis(15),
                    cache_config: IndexCacheConfig::disabled(),
                    ..Default::default()
                },
            )
            .unwrap();

        // Baseline unterminated row rolled back to provisional tail!
        let snap0 = watch.snapshot();
        assert_eq!(snap0.committed_rows, 1);
        assert_eq!(snap0.committed_record_end, 7);
        assert_eq!(snap0.provisional_tail.as_ref().unwrap().byte_len, 5);

        // Writer finishes row 1 ("tial\n") and appends row 2 ("2,other\n").
        let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"tial\n2,other\n").unwrap();
        writer.flush().unwrap();
        watch.nudge();

        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().committed_rows == 3
        }));
        assert_eq!(watch.snapshot().filter_matches_found, Some(1));
        let matches = watch.read_filtered_rows(0, 10).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].row, 1);
        assert_eq!(matches[0].fields[1], b"partial");

        let stopped = watch.stop().unwrap();
        assert_eq!(stopped.index.indexed_rows(), 3);
        assert_eq!(stopped.filter_index.unwrap().matches_found(), 1);
    }

    #[test]
    fn truncation_and_copytruncate_rewrite_trigger_alerts() {
        // Case 1: Truncation
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("trunc.csv");
            std::fs::write(&path, b"id,v\n1,alpha\n2,beta\n").unwrap();
            let (session, index) =
                open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
            let watch = session
                .start_live_watch(
                    &index,
                    None,
                    WatchConfig {
                        poll_interval: Duration::from_millis(15),
                        cache_config: IndexCacheConfig::disabled(),
                        ..Default::default()
                    },
                )
                .unwrap();

            let file = FsOpenOptions::new().write(true).open(&path).unwrap();
            file.set_len(5).unwrap();
            watch.nudge();
            assert!(wait_until(Duration::from_secs(2), || {
                matches!(
                    watch.snapshot().status,
                    WatchStatus::Alert(WatchAlert::Truncated { .. })
                )
            }));
            let stopped = watch.stop().unwrap();
            assert!(matches!(
                stopped.final_status,
                WatchStatus::Alert(WatchAlert::Truncated { .. })
            ));
        }

        // Case 2: Copytruncate / same-size in-place seam overwrite
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("rewrite.csv");
            std::fs::write(&path, b"id,v\n1,alpha\n2,beta\n").unwrap();
            let (session, index) =
                open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
            let watch = session
                .start_live_watch(
                    &index,
                    None,
                    WatchConfig {
                        poll_interval: Duration::from_millis(15),
                        cache_config: IndexCacheConfig::disabled(),
                        ..Default::default()
                    },
                )
                .unwrap();

            // Overwrite same length in place (e.g. copytruncate + equal/larger regrow)
            let mut file = FsOpenOptions::new().write(true).open(&path).unwrap();
            file.write_all(b"id,v\n9,ZZZZZ\n8,YYYY\n3,more\n").unwrap();
            file.flush().unwrap();
            watch.nudge();
            assert!(wait_until(Duration::from_secs(2), || {
                matches!(
                    watch.snapshot().status,
                    WatchStatus::Alert(WatchAlert::Rewritten)
                )
            }));
            let stopped = watch.stop().unwrap();
            assert_eq!(
                stopped.final_status,
                WatchStatus::Alert(WatchAlert::Rewritten)
            );
        }
    }

    #[test]
    fn rename_rotation_drains_old_fd_then_signals_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.csv");
        let rotated_path = dir.path().join("app.csv.1");
        std::fs::write(&path, b"id,msg\n1,first\n").unwrap();

        let (session, index) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let watch = session
            .start_live_watch(
                &index,
                None,
                WatchConfig {
                    poll_interval: Duration::from_millis(15),
                    cache_config: IndexCacheConfig::disabled(),
                    ..Default::default()
                },
            )
            .unwrap();

        // Keep an open writer handle to the original inode, rename the file away, write one more
        // row to the old inode, then create a new file at the original path!
        let mut old_writer = FsOpenOptions::new().append(true).open(&path).unwrap();
        std::fs::rename(&path, &rotated_path).unwrap();
        old_writer.write_all(b"2,drained_from_old_fd\n").unwrap();
        old_writer.flush().unwrap();
        watch.nudge();

        assert!(wait_until(Duration::from_secs(2), || {
            let s = watch.snapshot();
            s.committed_rows == 3 && s.status == WatchStatus::FollowingUnlinked
        }));

        // Now append one final row to the old inode AND create the replacement file at `path`
        // BEFORE nudging so the simultaneous drain-on-Replaced path is exercised directly.
        old_writer.write_all(b"3,final_before_rotate\n").unwrap();
        old_writer.flush().unwrap();
        std::fs::write(&path, b"id,msg\n100,new_file\n").unwrap();
        watch.nudge();

        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().status == WatchStatus::Alert(WatchAlert::Rotated)
        }));

        // Both old-FD drained rows must be committed and readable!
        assert_eq!(watch.snapshot().committed_rows, 4);
        let rows = watch.read_rows(2, 2).unwrap();
        assert_eq!(rows[0].fields[1], b"drained_from_old_fd");
        assert_eq!(rows[1].fields[1], b"final_before_rotate");

        let stopped = watch.stop().unwrap();
        assert_eq!(
            stopped.final_status,
            WatchStatus::Alert(WatchAlert::Rotated)
        );
    }

    #[test]
    fn pending_tail_exceeding_max_record_bytes_surfaces_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge_tail.csv");
        std::fs::write(&path, b"a,b\n").unwrap();
        let (session, index) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let watch = session
            .start_live_watch(
                &index,
                None,
                WatchConfig {
                    poll_interval: Duration::from_millis(15),
                    max_record_bytes: 128,
                    cache_config: IndexCacheConfig::disabled(),
                    ..Default::default()
                },
            )
            .unwrap();

        let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(&[b'x'; 200]).unwrap();
        writer.flush().unwrap();
        watch.nudge();

        assert!(wait_until(Duration::from_secs(2), || {
            matches!(
                watch.snapshot().status,
                WatchStatus::Alert(WatchAlert::ParseError(_))
            )
        }));
        let stopped = watch.stop().unwrap();
        assert!(matches!(
            stopped.final_status,
            WatchStatus::Alert(WatchAlert::ParseError(_))
        ));
    }

    #[test]
    fn sustained_high_rate_appender_stress_and_incremental_filter_parity() {
        // Sustained >1,000,000 rows/min rate (>= 16,667 rows/sec): append 50,000 rows across
        // 50 rapid bursts with coalesced notifications, active filter, and independent oracle check.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stress_watch.csv");
        std::fs::write(&path, b"id,level,amount\n").unwrap();

        let (session, index) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let query = FilterQuery::single(1, FilterOperator::Equals, b"WARN".to_vec());
        let filter = session.start_filter(query.clone()).unwrap().wait().unwrap();

        let watch = session
            .start_live_watch(
                &index,
                Some(&filter),
                WatchConfig {
                    poll_interval: Duration::from_millis(10),
                    cache_config: IndexCacheConfig::disabled(),
                    ..Default::default()
                },
            )
            .unwrap();

        let total_appended = 100_000u64;
        let burst_size = 1_000u64;
        let mem_before = crate::process_memory_usage().map(|sample| sample.bytes);
        let started = Instant::now();
        let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
        let mut buf = String::with_capacity(32 * 1024);
        for burst in 0..(total_appended / burst_size) {
            buf.clear();
            for offset in 0..burst_size {
                let id = burst * burst_size + offset + 1;
                let level = if id.is_multiple_of(10) {
                    "WARN"
                } else {
                    "INFO"
                };
                use std::fmt::Write as _;
                let _ = writeln!(&mut buf, "{id},{level},{}.50", id % 1000);
            }
            writer.write_all(buf.as_bytes()).unwrap();
            writer.flush().unwrap();
            if burst % 25 == 24 {
                thread::sleep(Duration::from_millis(2));
            }
        }
        watch.nudge();

        assert!(wait_until(Duration::from_secs(5), || {
            watch.snapshot().committed_rows == total_appended + 1
        }));
        let elapsed = started.elapsed();
        let rows_per_min = (total_appended as f64 / elapsed.as_secs_f64().max(0.001)) * 60.0;
        assert!(
            rows_per_min >= 1_000_000.0,
            "expected >= 1M rows/min ingestion, measured {rows_per_min:.0} rows/min ({elapsed:?})"
        );
        if let (Some(before), Some(after)) = (
            mem_before,
            crate::process_memory_usage().map(|sample| sample.bytes),
        ) {
            let delta = after.saturating_sub(before);
            assert!(
                delta < 64 * 1024 * 1024,
                "sustained Live Watch RSS delta must stay bounded (< 64 MiB), grew by {delta} bytes"
            );
        }
        assert_eq!(watch.snapshot().filter_matches_found, Some(10_000));

        let stopped = watch.stop().unwrap();
        assert_eq!(stopped.final_status, WatchStatus::Stopped);
        assert_eq!(stopped.index.indexed_rows(), total_appended + 1);
        let watched_filter = stopped.filter_index.unwrap();
        assert_eq!(watched_filter.matches_found(), 10_000);

        // Verify exact match parity against a fresh independent static open + filter.
        let (oracle_session, oracle_index) =
            open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        assert_eq!(stopped.index.indexed_rows(), oracle_index.indexed_rows());
        let oracle_filter = oracle_session.start_filter(query).unwrap().wait().unwrap();
        assert_eq!(
            watched_filter.matches_found(),
            oracle_filter.matches_found()
        );
        assert_eq!(
            stopped
                .session
                .read_filtered_rows(&watched_filter, 9_995, 5)
                .unwrap(),
            oracle_session
                .read_filtered_rows(&oracle_filter, 9_995, 5)
                .unwrap()
        );
    }

    #[test]
    fn clean_stop_refreshes_sidecar_once_and_never_persists_uncommitted_tail() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("private-cache");
        let path = dir.path().join("sidecar_watch.csv");
        let mut initial = String::from("id,val\n");
        for i in 1..=200 {
            initial.push_str(&format!("{i},row-{i:04}\n"));
        }
        std::fs::write(&path, &initial).unwrap();

        let cache_config = IndexCacheConfig {
            enabled: true,
            cache_dir: Some(cache_dir.clone()),
            min_file_bytes: 0,
            max_cache_bytes: 16 * 1024 * 1024,
        };
        let (session, index) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let watch = session
            .start_live_watch(
                &index,
                None,
                WatchConfig {
                    poll_interval: Duration::from_millis(15),
                    cache_config: cache_config.clone(),
                    ..Default::default()
                },
            )
            .unwrap();

        // Append an uncommitted partial row -> cache_dir must remain absent/empty while watching!
        let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"201,pending_tail").unwrap();
        writer.flush().unwrap();
        watch.nudge();
        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().provisional_tail.is_some()
        }));
        assert!(!cache_dir.exists());

        // Terminate the row and stop cleanly -> sidecar saves once and hits cleanly on reopen.
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();
        watch.nudge();
        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().committed_rows == 202
        }));
        let stopped = watch.stop().unwrap();
        assert_eq!(stopped.final_status, WatchStatus::Stopped);

        // Reopening with the same cache_config should get a validated sidecar cache hit.
        let reopened = Session::open(&path, OpenOptions::default()).unwrap();
        let reindexed = reopened
            .start_indexing_with_options(IndexConfig::default(), Some(1), cache_config)
            .unwrap()
            .wait()
            .unwrap();
        assert!(reindexed.telemetry().cache_hit);
        assert_eq!(reindexed.indexed_rows(), 202);
    }

    #[test]
    fn same_size_provisional_tail_rewrite_alerts_and_never_persists_wrong_sidecar() {
        use std::io::{Seek, SeekFrom};

        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("private-cache");
        let path = dir.path().join("provisional_rewrite.csv");
        let initial = b"id\n1\n";
        std::fs::write(&path, initial).unwrap();

        let cache_config = IndexCacheConfig {
            enabled: true,
            cache_dir: Some(cache_dir.clone()),
            min_file_bytes: 0,
            max_cache_bytes: 16 * 1024 * 1024,
        };
        let (session, index) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let watch = session
            .start_live_watch(
                &index,
                None,
                WatchConfig {
                    poll_interval: Duration::from_millis(500),
                    force_polling: true,
                    cache_config: cache_config.clone(),
                    ..Default::default()
                },
            )
            .unwrap();

        // Append unterminated 3-byte provisional tail `abc` and let watcher consume it.
        let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"abc").unwrap();
        writer.flush().unwrap();
        watch.nudge();
        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().scanner_consumed_bytes == (initial.len() + 3) as u64
        }));
        assert_eq!(watch.snapshot().committed_rows, 2);

        // Overwrite those exact 3 provisional bytes in place with `a\nb` (same length, 2 rows!),
        // then immediately stop: must detect `Rewritten` and NEVER write a bogus sidecar!
        let mut rw = FsOpenOptions::new().write(true).open(&path).unwrap();
        rw.seek(SeekFrom::Start(initial.len() as u64)).unwrap();
        rw.write_all(b"a\nb").unwrap();
        rw.flush().unwrap();

        let stopped = watch.stop().unwrap();
        assert_eq!(
            stopped.final_status,
            WatchStatus::Alert(WatchAlert::Rewritten)
        );
        assert!(!stopped.index.is_complete());
        assert!(!cache_dir.exists());

        // Fresh reopen with the same cache_config sees the true 4 rows (header + 1 + a + b).
        let reopened = Session::open(&path, OpenOptions::default()).unwrap();
        let reindexed = reopened
            .start_indexing_with_options(IndexConfig::default(), Some(1), cache_config)
            .unwrap()
            .wait()
            .unwrap();
        assert!(!reindexed.telemetry().cache_hit);
        assert_eq!(reindexed.indexed_rows(), 4);
    }

    #[test]
    fn attach_filter_during_concurrent_commit_preserves_every_match_and_ordinal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent_filter.csv");
        std::fs::write(&path, b"id,tag\n1,HIT\n2,MISS\n").unwrap();

        let (session, index) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let watch = session
            .start_live_watch(
                &index,
                None,
                WatchConfig {
                    poll_interval: Duration::from_millis(15),
                    cache_config: IndexCacheConfig::disabled(),
                    ..Default::default()
                },
            )
            .unwrap();

        let path_for_hook = path.clone();
        let shared_for_hook = Arc::clone(&watch.shared);
        let query = FilterQuery::single(1, FilterOperator::Equals, b"HIT".to_vec());
        // Hook runs AFTER initial scan of `[0..initial_end)` and BEFORE atomic installation:
        // append a matching row and wait for the watcher to commit it into `shared.index`!
        let attached = watch
            .attach_filter_query_with_hook(query.clone(), move || {
                let mut writer = FsOpenOptions::new()
                    .append(true)
                    .open(&path_for_hook)
                    .unwrap();
                writer.write_all(b"3,HIT\n4,MISS\n").unwrap();
                writer.flush().unwrap();
                shared_for_hook.signal_wakeup();
                assert!(wait_until(Duration::from_secs(2), || {
                    shared_for_hook.snapshot.read().unwrap().committed_rows == 5
                }));
            })
            .unwrap();

        // Both row 1 (before hook) and row 3 (committed mid-attach) must be captured!
        assert_eq!(attached.matches_found(), 2);

        // Append one more matching row AFTER attach -> incremental path captures row 5 too!
        let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"5,HIT\n").unwrap();
        writer.flush().unwrap();
        watch.nudge();
        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().committed_rows == 6 && watch.snapshot().filter_matches_found == Some(3)
        }));

        let stopped = watch.stop().unwrap();
        assert_eq!(stopped.final_status, WatchStatus::Stopped);
        let watched_filter = stopped.filter_index.unwrap();
        let (oracle_session, _) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let oracle_filter = oracle_session.start_filter(query).unwrap().wait().unwrap();
        assert_eq!(
            stopped
                .session
                .read_filtered_rows(&watched_filter, 0, 10)
                .unwrap(),
            oracle_session
                .read_filtered_rows(&oracle_filter, 0, 10)
                .unwrap()
        );
    }

    #[test]
    fn utf16_odd_and_even_truncation_returns_source_changed_across_all_readers() {
        for truncate_to_odd in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir
                .path()
                .join(format!("utf16_trunc_{truncate_to_odd}.csv"));
            let mut content = Encoding::Utf16Le.bom().to_vec();
            content.extend_from_slice(
                &Encoding::Utf16Le
                    .encode(b"id,tag\n1,HIT\n2,HIT\n3,HIT\n", 4096)
                    .unwrap(),
            );
            std::fs::write(&path, &content).unwrap();

            let (session, index) =
                open_and_index(&path, Some(Encoding::Utf16Le), HeaderMode::FirstRow);
            let query = FilterQuery::single(1, FilterOperator::Equals, b"HIT".to_vec());
            let initial_filter = session.start_filter(query.clone()).unwrap().wait().unwrap();
            // Long polling interval so the worker stays idle while we test direct readers on the
            // truncated file descriptor.
            let watch = session
                .start_live_watch(
                    &index,
                    Some(&initial_filter),
                    WatchConfig {
                        poll_interval: Duration::from_secs(60),
                        force_polling: true,
                        cache_config: IndexCacheConfig::disabled(),
                        ..Default::default()
                    },
                )
                .unwrap();

            // Truncate below committed_record_end to either an odd byte count (e.g. 1 byte) or an
            // even byte count inside row 1.
            let trunc_len = if truncate_to_odd { 1u64 } else { 18u64 };
            let f = FsOpenOptions::new().write(true).open(&path).unwrap();
            f.set_len(trunc_len).unwrap();

            // 1. Unfiltered reader must return SourceChanged (never a torn record or hang).
            assert!(matches!(
                watch.read_rows(0, 4),
                Err(QuarryError::SourceChanged)
            ));
            // 2. Filtered reader must return SourceChanged (never silently return partial matches).
            assert!(matches!(
                watch.read_filtered_rows(0, 3),
                Err(QuarryError::SourceChanged)
            ));
            // 3. Filter attachment must return SourceChanged immediately (never spin on odd byte!).
            assert!(matches!(
                watch.attach_filter_query(query),
                Err(QuarryError::SourceChanged)
            ));

            let stopped = watch.stop().unwrap();
            assert!(matches!(
                stopped.final_status,
                WatchStatus::Alert(WatchAlert::Truncated { .. })
            ));
        }
    }

    #[test]
    fn malformed_append_right_before_stop_propagates_parse_error_and_incomplete_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("malformed_before_stop.csv");
        std::fs::write(&path, b"id,v\n1,ok\n").unwrap();

        let (session, index) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let watch = session
            .start_live_watch(
                &index,
                None,
                WatchConfig {
                    poll_interval: Duration::from_secs(60),
                    force_polling: true,
                    max_record_bytes: 32,
                    cache_config: IndexCacheConfig::disabled(),
                },
            )
            .unwrap();

        // Append an oversized row immediately before calling `stop()` without nudging first.
        let mut writer = FsOpenOptions::new().append(true).open(&path).unwrap();
        writer
            .write_all(b"2,aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n")
            .unwrap();
        writer.flush().unwrap();

        let stopped = watch.stop().unwrap();
        assert!(matches!(
            stopped.final_status,
            WatchStatus::Alert(WatchAlert::ParseError(_))
        ));
        assert!(!stopped.index.is_complete());
        assert!(stopped.session.completed_record_count().is_none());
    }

    #[test]
    fn row_zero_corruption_still_delivers_rewritten_alert_on_stop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt_row0.csv");
        std::fs::write(&path, b"id,v\n1,ok\n").unwrap();

        let (session, index) = open_and_index(&path, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let watch = session
            .start_live_watch(
                &index,
                None,
                WatchConfig {
                    poll_interval: Duration::from_millis(15),
                    cache_config: IndexCacheConfig::disabled(),
                    ..Default::default()
                },
            )
            .unwrap();

        // Overwrite row 0 in place with malformed unclosed quote CSV (same length -> seam mismatch).
        let mut f = FsOpenOptions::new().write(true).open(&path).unwrap();
        f.write_all(b"\"unclosed\n1,ok\n").unwrap();
        f.flush().unwrap();
        watch.nudge();

        assert!(wait_until(Duration::from_secs(2), || {
            watch.snapshot().status == WatchStatus::Alert(WatchAlert::Rewritten)
        }));
        let stopped = watch.stop().expect("stop must still deliver alert cleanly");
        assert_eq!(
            stopped.final_status,
            WatchStatus::Alert(WatchAlert::Rewritten)
        );
        assert!(!stopped.index.is_complete());
    }

    #[test]
    fn start_live_watch_rejects_partial_or_cross_session_filter_index() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.csv");
        let path_b = dir.path().join("b.csv");
        std::fs::write(&path_a, b"id,v\n1,a\n2,b\n").unwrap();
        std::fs::write(&path_b, b"id,v\n1,a\n2,b\n").unwrap();

        let (session_a, index_a) =
            open_and_index(&path_a, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let (session_b, _) = open_and_index(&path_b, Some(Encoding::Utf8), HeaderMode::FirstRow);
        let query = FilterQuery::single(1, FilterOperator::Equals, b"a".to_vec());

        // 1. Cross-session FilterIndex must be rejected.
        let filter_b = session_b
            .start_filter(query.clone())
            .unwrap()
            .wait()
            .unwrap();
        assert!(filter_b.is_complete());
        assert!(matches!(
            session_a.start_live_watch(&index_a, Some(&filter_b), WatchConfig::default()),
            Err(QuarryError::InvalidOption(_))
        ));

        // 2. Bounded/paused (incomplete) FilterIndex must be rejected.
        let bounded_filter = session_a
            .start_filter_with_edits_and_limit(query, Arc::default(), Some(9))
            .unwrap()
            .wait()
            .unwrap();
        assert!(!bounded_filter.is_complete());
        assert!(matches!(
            session_a.start_live_watch(&index_a, Some(&bounded_filter), WatchConfig::default()),
            Err(QuarryError::InvalidOption(_))
        ));
    }
}
