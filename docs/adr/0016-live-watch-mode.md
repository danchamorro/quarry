# ADR 0016: Opt-In Append-Only Live Watch Mode

**Status:** Accepted (`2026-11-24`, Owner sign-off `T12`)
**Date:** 2026-11-26
**Related:** [ADR 0001: Initial engine architecture](0001-initial-engine.md), [ADR 0005: Temporary storage](0005-temporary-storage.md), [ADR 0007: Document tabs and dormancy](0007-document-tabs.md), [ADR 0012: Compressed delimited files](0012-compressed-delimited-files.md)

## Context

Quarry's default source-modification guard (`SourceStamp`) intentionally treats any external change to an open file as a conflict (`QuarryError::SourceChanged`), freezing navigation and writes until the user explicitly reloads from disk. That strict contract protects editing and exports from silent corruption, but users tailing active log pipelines, ETL exports, or streaming CSV/TSV outputs need a way to watch newly appended rows arrive live without repeatedly reloading or losing their scroll position.

Appending writers introduce several correctness hazards that a naive `tail` loop breaks:

1. **Partial or unterminated trailing records:** A writer may flush mid-field, mid-quoted-string, between `\r` and `\n`, in the middle of a multi-byte UTF-8 character, on an odd UTF-16 byte, or between a UTF-16 high and low surrogate pair. Even an ordinary unquoted trailing row without `\n` can still be extended by the next write.
2. **External truncation, copytruncate, and log rotation:** Writers or log rotators may truncate a file in place, overwrite it via `copytruncate`, or rename it and create a new file at the same path.
3. **Active filters and memory bounds:** Re-scanning a multi-gigabyte file from byte 0 on every append or buffering unbounded incomplete tails would violate Quarry's bounded-RAM and responsiveness guarantees.
4. **Sidecar cache pollution and FD exhaustion:** Persisting raw uncommitted tail bytes into structural sidecars or leaving uncounted OS watcher descriptors open across tabs would compromise privacy and Quarry's `< 128` process FD budget.

## Decision

Introduce an explicit, opt-in, append-only **Live Watch Mode** (`LiveWatchJob` in `crates/quarry-core/src/watch.rs`) with an `"append expected"` source-guard mode strictly separate from normal `source_changed` handling.

### 1. Eligibility & Mutual Exclusion

- Live Watch is opt-in and eligible only for **clean**, **uncompressed** (`.csv.gz` rejected), **whole-file** (bounded sections rejected) documents whose initial `StructuralIndex` reached validated EOF (`index.is_complete()`), with no active write, structural, or search jobs and no existing `source_changed` conflict.
- While Live Watch is active, the tab enters an explicit `"append expected"` mode and is **pinned** (`PinReason::LiveWatching`, with its OS descriptors explicitly accounted in `FdBudgetLedger`).
- All edits, Undo/Redo, structural transforms, Save / Save As / Save As UTF-8, Replace All, Rename Header, Reload, Open Section, and Format changes are **mutually exclusive** with Live Watch and remain disabled until the user stops watching.
- Stopping Live Watch cleanly transitions the document back to normal `SourceStamp`-guarded mode at the latest reconciled file state, finalizing any valid unterminated EOF record and refreshing the optional persistent sidecar cache once via ordinary validated cache rules.

### 2. Three Separate Frontiers & Provisional Tail Record

Every `LiveWatchJob` tracks three distinct monotonic byte frontiers on the pinned open file descriptor:

1. **`observed_file_len: u64`:** Latest `fstat` byte length of the pinned descriptor.
2. **`scanner_consumed_bytes: u64`:** Total bytes consumed from the pinned descriptor into the incremental tail scanner (`<= observed_file_len`).
3. **`committed_record_end: u64`:** Byte offset immediately after the last newline-terminated logical record committed to `StructuralIndex` (`<= scanner_consumed_bytes`).

Rules governing the uncommitted tail `[committed_record_end, observed_file_len)`:

- **Provisional final record:** Bytes after `committed_record_end` are never committed into `StructuralIndex` while watching continues. Even an ordinary unquoted trailing line without a newline terminator remains provisional because the writer may still append characters or columns to it.
- **Unterminated baseline row rollback:** If the file did not end with a newline terminator when Live Watch began, the initial committed frontier rolls back by that single trailing record (leaving it in the provisional tail state) so completing that line later commits one intact record rather than splitting or duplicating it.
- **Stateful cross-read carry:** The tail scanner carries complete parser and encoding state across arbitrary read boundaries: quote/escape state (`Quoted`, `QuotedEscape`, `AfterQuote`), trailing `\r` (`AfterQuoteCr` and unquoted `\r`), incomplete multi-byte UTF-8 sequences, an odd trailing UTF-16 byte, and pending UTF-16 high surrogates.
- **Bounded 64 MiB tail cap:** Pending uncommitted tail bytes (`observed_file_len - committed_record_end`) are capped at `DEFAULT_MAX_RECORD_BYTES` (`64 MiB`). Exceeding the cap stops Live Watch immediately and surfaces `QuarryError::RecordTooLarge`.
- **Clean stop finalization:** On clean user stop (not truncation/rewrite/error), any non-empty valid provisional tail at EOF is finalized using ordinary RFC 4180 EOF rules so the stopped `StructuralIndex` matches a full static index byte-for-byte.

### 3. OS Notifications, Coalescing & Seam Reconciliation

- **Notification backends (zero new third-party dependencies):**
  - **macOS:** `kqueue` + `EVFILT_VNODE` (`NOTE_EXTEND | NOTE_WRITE | NOTE_DELETE | NOTE_RENAME | NOTE_ATTRIB | NOTE_REVOKE`) on the pinned file descriptor, paired with a wakeup pipe for instant shutdown.
  - **Linux:** `inotify` (`IN_MODIFY | IN_ATTRIB | IN_DELETE_SELF | IN_MOVE_SELF` on the file plus `IN_MOVED_TO | IN_CREATE` on its parent directory for rotation detection), multiplexed with a wakeup pipe via `poll`.
  - **Network volumes and other OSes:** Bounded polling (`250 ms`–`500 ms`) automatically selected when `is_network_filesystem` detects NFS, SMB/CIFS, AFP, WebDAV, FUSE, 9P, Ceph, or remote mounts.
- **Wakeup coalescing & full reconciliation:** Notifications are treated purely as wakeups. Multiple queued events coalesce into one bounded reconciliation pass over the pinned open FD:
  1. **Size & truncation check:** `fstat` the pinned FD. If `current_len < observed_file_len` or `current_len < committed_record_end`, or if any `pread` returns fewer bytes than expected (`short read`), stop immediately and surface a **Truncated** alert offering **Reload from Disk**.
  2. **Committed tail seam check:** Re-read the last `min(committed_record_end, 4096)` bytes ending at `committed_record_end` from the pinned FD and compare byte-for-byte against the saved seam buffer. Any mismatch (e.g., `copytruncate` or tail overwrite) stops Live Watch immediately and surfaces a **Rewritten** alert offering **Reload from Disk**.
  3. **Path identity & rename rotation (`tail -f` semantics):** Inspect the current path's OS file identity (`(dev, ino)` on Unix, `(volume_serial, file_id)` on Windows).
     - If the file was renamed away and no replacement exists yet (`NotFound`), continue draining appends from the open FD while surfacing an unlinked/moved status.
     - If a new file with a different identity appears at `path`, drain any remaining valid bytes on the old open FD, then stop Live Watch and surface a **Rotated** alert offering **"Open new file"**.
- **Documented boundary:** An append-only tail watcher cannot detect a silent in-place overwrite strictly earlier than the `4 KiB` tail seam that preserves total file size, nor a complete truncate-and-regrow between wakeups that reproduces identical tail seam bytes at the same length. Users needing full-file integrity checks can stop Live Watch and reload.

### 4. Incremental Active Filters, Throttled UI & Accessibility

- **Incremental filtering:** When an active `FilterIndex` exists during Live Watch, newly committed records `[old_rows..new_rows)` are evaluated incrementally against the compiled `FilterQuery` matchers and appended to `FilterIndex` without rescanning earlier rows. Filtered viewport reads use the watch's pinned open FD bounded by `committed_record_end`.
- **Repaint throttling & scroll-preserving tail follow:** UI updates throttle at ~10 Hz (`100 ms` cadence) during sustained streams while surfacing state transitions immediately. Scrolling up pauses tail-follow (`tail_follow = false`) without disrupting ingestion or jumping the viewport; scrolling back to the bottom or activating **Follow Tail** resumes tail-follow.
- **Sidecar hygiene:** Live Watch never persists raw uncommitted tail bytes to disk or sidecar caches. Sidecars are updated at most once on clean watch stop via ordinary cache validation.
- **Quiet accessibility:** Polling counters render inside a quiet `Role::Group` region; VoiceOver and Linux AT-SPI live announcements fire only on discrete state transitions (started, tail paused/resumed, stopped, rotated, truncated, rewritten, error).

## Consequences

- Users can monitor active CSV/TSV writers at sustained high throughput (tested at `>= 1M rows/min`) with flat bounded memory, zero scroll jumps while inspecting earlier rows, and incremental filter updates.
- Quarry's strict `SourceStamp` protection for editing and saving remains completely intact because Live Watch is strictly opt-in, read-only, and mutually exclusive with modifications.
- No new third-party crates are added to `Cargo.lock` or bundled license notices.
