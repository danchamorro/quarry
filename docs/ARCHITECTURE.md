# Quarry Architecture

## Objective
Quarry must operate on files larger than available RAM. Treat it as a **large-file data engine with a desktop interface**, not a conventional text editor that happens to parse CSV.

## Current design
```text
egui viewer / CLI / AppKit comparator
   |
quarry-core
   +-- Session and format detection
   +-- Structural index worker
   +-- Bounded viewport reads
   +-- Literal search worker
   +-- Bounded filter-index worker
   +-- Bounded filtered-row read worker
   +-- Streaming filtered-export worker
   +-- Streaming edited Save and Save As worker
   |
quarry-delimited
   +-- Record scanner
   +-- Delimited record parser
```

The egui app is the selected production UI. The CLI exposes the engine and
benchmark commands, while AppKit remains a measured comparator. The Rust engine
stays UI-independent so new filter operators, transformations, sorting, and
export can reuse the same boundaries.

## Current workspace
```text
quarry/
  crates/
    quarry-delimited/
    quarry-core/
  apps/
    quarry-cli/
    quarry-egui/
    quarry-appkit/
  fixtures/
  docs/
```

`quarry-cli` provides the `quarry` and `quarry-bench` binaries. File access,
structural indexing, bounded viewport reads, literal search, and filtering
remain modules in core until their APIs or dependencies justify separate
crates. See
[ADR 0001](adr/0001-initial-engine.md).

## File access
Core uses `std::fs::File`, fixed-size sequential reads, and seeks from the
nearest structural checkpoint for viewport access. Memory mapping remains
deferred until measurements show a benefit that justifies the dependency and
platform tradeoffs.

## Progressive open
Bootstrap by sampling a bounded region and parsing enough rows for the first viewport. Return that viewport immediately. Continue structural indexing in background workers.

## Structural indexing
Delimited files cannot be indexed correctly by blindly finding newline bytes because quoted fields may contain line breaks. The indexer must track dialect and quote state.

By default, the current index scans 1 MiB chunks and starts with one byte-offset
checkpoint per 4,096 records. Checkpoints have a 16 MiB default budget; when
full, the interval doubles and existing checkpoints compact. Persistent sidecar
indexes and their invalidation rules remain deferred.

## Parsing
Optimize common paths without compromising quoted fields, escaped quotes, embedded delimiters/newlines, CRLF/LF, UTF-8 boundaries, malformed records, wide rows, or giant fields. Avoid allocating an owned string for every cell; materialize only what the viewport needs.

## Viewport
The engine seeks from the nearest checkpoint and parses every field in a
bounded row range. The egui viewer's active viewport buffer retains the visible
rows plus two rows of overscan on each side and renders at most 32 columns at a
time. Column windowing limits UI work; it does not project fields out of the
parsed rows.

## Column views
The viewer keeps source column indexes as the canonical identity for search,
selection, and copy. A UI-only column view stores display order, hidden state,
the first shown position, and a visible list capped at 32 source columns.
View, hide/show, arbitrary move-to-position, manager-only drag, first-columns,
and reset actions update this metadata without changing parsed rows or output
order. Drag handles exist only in the Columns manager; main-grid headers remain
resize-only. A search match automatically shows and centers its source column,
while row copy continues to serialize every source field in file order.

View order and hidden state remain non-dirty UI metadata. They affect saved
output only through an explicit transformed-output choice.

Header columns are known immediately. If a later ragged row contains more
fields, the viewer appends those newly known source columns without resetting
the existing layout. Discovering the maximum width of an entire headerless
ragged file would require a separate full scan and remains deferred.

## Clipboard copying
The viewer retains a selected cell only while its row and column remain visible,
and a selected row only while that row remains visible. Cell and row copy read
the sparse edit overlay before falling back to the bounded source row. Row copy
serializes every actual field as UTF-8 TSV in source-column order, quoting
fields with tabs, line breaks, or quotes and doubling embedded quotes. It
excludes the header and synthetic row number. Invalid UTF-8 is reported instead
of replaced, and a 64 MiB output limit prevents clipboard serialization from
growing without bound.

## Bounded memory
Core defaults to 1 MiB read chunks, with a 64 MiB bootstrap and record limit and
fixed budgets for the adaptive structural and filter indexes. The UI retains
the bounded bootstrap rows, one source viewport buffer, one filtered viewport
buffer when filtering is active, compact metadata per known column, one search
match, and at most a 64 MiB clipboard payload.
[ADR 0002](adr/0002-defer-viewport-cache.md) records why an application viewport
cache remains deferred.

Unsaved edits use a sparse overlay whose memory grows with the number and size
of user edits, not with source-file size. Save and Save As enforce the existing
maximum record size against every fully serialized edited record before
publication. For a fixed sparse edit set, the streaming worker retains a fixed
read chunk, at most one bounded record and its decoded fields, and the overlay.

## Concurrency
Rust workers currently handle indexing, literal search, filtering, filtered
viewport reads, filtered export, and edited Save and Save As. Each publishes progress
and supports cancellation.
Jobs normally join before they are dropped. Rapid filtered navigation cancels
obsolete reads, keeps only the newest pending window, and joins a cancelled read
after it finishes. Filter resets and document lifecycle changes detach an active
read-only viewport worker so the render thread never waits for cleanup; each
worker owns its resources and exits at its next cancellation check. An active
filtered export, Save, or Save As blocks document replacement until cancellation
finishes. App shutdown joins active output workers so temporary-output cleanup
is guaranteed.

## Search and filtering
Literal Find Next uses a cancellable core worker after structural indexing. It
starts at the nearest row checkpoint, scans fixed 1 MiB chunks with the shared
delimited-record scanner, parses one bounded record at a time, and retains only
the first decoded-cell match. The job publishes byte/row progress and joins its
worker on wait or drop. Memory therefore depends on the query, the fixed chunk,
the 64 MiB maximum record and its decoded fields, one match, and the bounded
structural index, not on file size or match count.

Search and filter workers intentionally scan the immutable source rather than
the unsaved overlay. The viewer therefore disables new searches and filters
while data-cell edits exist and requires an active filter to be cleared before
cell editing. This keeps displayed results from silently disagreeing with the
unsaved document. Overlay-aware search and filtering remain a later slice.

A `FilterQuery` owns one or more `FilterPredicate` values. Each predicate stores
a source column, a case-sensitive contains or equality operator, and its literal
value. All predicates use AND semantics. The scanner parses each bounded record
once, then evaluates every predicate against the decoded fields. A missing
column or any failed predicate rejects that row. `FilterQuery::single` keeps
single-predicate callers compatible with the same path.

The sequential worker counts every matching row while retaining only adaptive
match checkpoints under a fixed budget. When the budget fills, the checkpoint
interval grows and existing checkpoints compact; the exact match count is
preserved.

Filtered navigation asks for a bounded match range. Core starts a cancellable
background read from the nearest retained match checkpoint, resumes the same
query, and materializes only the requested rows. Rapid navigation
cancels obsolete work and keeps only the newest requested window. The filter
index owns its query so a caller cannot accidentally read its checkpoints with
different filter semantics. Memory therefore depends on the predicate values,
fixed chunk and index budgets, the 64 MiB maximum record and its decoded fields,
and the requested row count, not on file size or match count.

## Filtered export
Filtered export scans the source once with the same decoded-cell predicate
semantics as filtered navigation, but copies each matching raw record to a
buffered temporary file. This preserves the source header, delimiter, quoting,
line endings, and multiline records byte for byte without retaining matching
rows. The worker publishes scanned bytes, parsed records, written rows, written
bytes, elapsed time, and cancellation state.

The temporary file is created beside the destination. A successful worker
flushes and syncs it before publishing the destination without overwriting an
existing path. Cancellation or failure removes the temporary file and never
publishes the destination. The source path itself is rejected as a destination.

## Planned sorting
Use a disk-aware external merge sort: bounded runs, sort in memory, spill to temporary storage, then merge into a stable row-order abstraction. Sorting must not delay the first performance milestone.

## Document editing and persistence

Editing occurs directly in the grid. The source file remains immutable while
the document is open. A sparse overlay stores committed edits by stable source
identity. Header renames use the original source column. Data-cell edits use the
physical record row and original source column, independent of the current
column display order. Viewport rendering and copy read the overlay before
falling back to decoded source values. Cancelling an inline edit does not change
document state, and restoring the original bytes removes that overlay entry.

The first data-cell slice edits only existing valid UTF-8 fields. It accepts
multiline input, but does not create a missing field in a ragged row or replace
invalid UTF-8 with lossy text. Row insertion, deletion, split/join, and output
column transformations remain separate features.

A document is dirty while effective edits exist. Open, reopen, format changes,
and application close must not silently discard dirty state. Every write path
must consume the current document overlay or remain unavailable while the
document is dirty.

Save and Save As use a streaming rewrite rather than in-place record mutation.
Save As publishes a selected destination and leaves the previous source
unchanged. Save creates temporary output beside the current regular file,
preserves its standard permissions, flushes and syncs the temporary file,
checks for metadata-visible source changes when Save starts and immediately
before replacement, then atomically replaces the source. If a change is
detected, Quarry invalidates offset-backed navigation and requires discarding
the unsaved edits plus reopening the source. Final-path symbolic links are
rejected with guidance to use Save As. Cancellation observed before publication
or a write failure removes temporary output without Quarry replacing the source
or clobbering an existing destination.

The rewrite scans records once with the same quote-aware scanner used by the
other streaming workers. It copies every unedited record byte for byte. For an
edited record, it parses the bounded record, replaces the selected decoded
fields, and serializes all fields with the document delimiter plus that
record's original CRLF, LF, or absent final line ending. An edit targeting a
missing row or column, a parse failure in an edited record, or serialized output
above the limit fails before publication.

Rename-based Save preserves standard permission bits but does not explicitly
preserve ownership, ACLs, extended attributes or resource forks, or hard-link
identity; other hard links retain the old contents. The final source-stamp check
narrows, but does not eliminate, the race before replacement.

After a successful Save or Save As publication, Quarry opens the published file
as the current source and rebuilds offset-dependent indexes before clearing
dirty state. If an in-place Save succeeds but reloading fails, Quarry removes
the stale document from use and asks the user to reopen it. This is required
because even a header rename can shift every later byte offset. Unchanged
records remain byte-preserving where possible; edited records are serialized
with the document dialect.

## UI selection
[ADR 0003](adr/0003-select-egui-ui.md) records the measured egui and AppKit
bake-off. It selects egui while keeping core independent of the UI. AppKit stays
in the workspace so the comparison remains reproducible.

## Benchmarking
Generate deterministic 1 GB, 10 GB, 25 GB, and 50 GB datasets locally,
including multiline quoted fields, wide tables, and long fields. Use separate
malformed-record fixtures for parser correctness.

Track time-to-first-rows, memory, index/search/filter/export/save throughput,
scroll frame time, cache behavior, cancellation latency, and exact edited-output
validation.

## Architecture rule
**If a feature only works because the entire file fits in RAM, it is not a finished Quarry feature.**
