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
   |
quarry-delimited
   +-- Record scanner
   +-- Delimited record parser
```

The egui app is the selected production UI. The CLI exposes the engine and
benchmark commands, while AppKit remains a measured comparator. The Rust engine
stays UI-independent so future filters, transformations, sorting, and export can
reuse the same boundaries.

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
structural indexing, bounded viewport reads, and literal search remain modules
in core until their APIs or dependencies justify separate crates. See
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

Header columns are known immediately. If a later ragged row contains more
fields, the viewer appends those newly known source columns without resetting
the existing layout. Discovering the maximum width of an entire headerless
ragged file would require a separate full scan and remains deferred.

## Clipboard copying
The viewer retains a selected cell only while its row and column remain visible,
and a selected row only while that row remains visible. Cell copy uses the
complete decoded field from the bounded row buffer. Row copy serializes every
actual field as UTF-8 TSV, quoting fields with tabs, line breaks, or quotes and
doubling embedded quotes. It excludes the header and synthetic row number.
Invalid UTF-8 is reported instead of replaced, and a 64 MiB output limit
prevents clipboard serialization from growing without bound.

## Bounded memory
Core defaults to 1 MiB read chunks, with a 64 MiB bootstrap and record limit and
a 16 MiB adaptive structural-index budget. The UI retains the bounded bootstrap
rows, one active viewport window, compact metadata per known column, one search
match, and at most a 64 MiB clipboard payload.
[ADR 0002](adr/0002-defer-viewport-cache.md) records why an application viewport
cache remains deferred.

## Concurrency
Rust workers currently handle indexing and literal search. Both publish
progress, support cancellation, and join before their job is dropped. Future
long-running filters, sorting, and export must follow the same lifecycle rule.

## Search and planned filters
Literal Find Next uses a cancellable core worker after structural indexing. It
starts at the nearest row checkpoint, scans fixed 1 MiB chunks with the shared
delimited-record scanner, parses one bounded record at a time, and retains only
the first decoded-cell match. The job publishes byte/row progress and joins its
worker on wait or drop. Memory therefore depends on the query, the fixed chunk,
the 64 MiB maximum record and its decoded fields, one match, and the bounded
structural index, not on file size or match count.

The first slice is case-sensitive and does not wrap or collect results. Filters
can later use streaming predicates and compact row/chunk selection metadata
instead of copying matching rows.

## Planned sorting
Use a disk-aware external merge sort: bounded runs, sort in memory, spill to temporary storage, then merge into a stable row-order abstraction. Sorting must not delay the first performance milestone.

## Planned transformations
Model split/join/reorder/drop operations as a non-destructive pipeline ending in
streaming export. Save As can later write either an unchanged source-order copy
or transformed output using the arranged column order. Hiding a view column
alone does not remove it from output.

## UI selection
[ADR 0003](adr/0003-select-egui-ui.md) records the measured egui and AppKit
bake-off. It selects egui while keeping core independent of the UI. AppKit stays
in the workspace so the comparison remains reproducible.

## Benchmarking
Generate deterministic 1 GB, 10 GB, 25 GB, and 50 GB datasets locally,
including multiline quoted fields, wide tables, and long fields. Use separate
malformed-record fixtures for parser correctness.

Track time-to-first-rows, memory, index/search/export throughput, scroll frame time, cache behavior, and cancellation latency.

## Architecture rule
**If a feature only works because the entire file fits in RAM, it is not a finished Quarry feature.**
