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
   +-- Split analysis worker
   +-- Streaming structural materialization worker
   +-- External merge sort worker
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
rows plus 16 rows of overscan on each side and renders at most 64 columns at a
time. Horizontal windowing keeps every shown column reachable while limiting
UI work; it does not project fields out of the parsed rows.

## Column views
The viewer keeps source column indexes as the canonical identity for search,
selection, and copy. A UI-only column view stores display order, hidden state,
and the complete shown-column list. Search, hide/show, manager-only drag, and
reset actions update this metadata without changing parsed rows or output order.
Main-grid headers select columns and expose structural actions; resizing is
available when 64 or fewer columns are shown. A search match automatically shows
and centers its source column, while row copy continues to serialize every
source field in file order.

View order and hidden state remain non-dirty UI metadata and never affect saved
output. Move Selected Columns and Delete Selected Columns are separate document
operations and do not consult the view metadata.

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
buffer when filtering is active, compact metadata per known column, the
user-driven Find navigation history, and at most a 64 MiB clipboard payload.
[ADR 0002](adr/0002-defer-viewport-cache.md) records why an application viewport
cache remains deferred.

Before a materialized operation, unsaved cell and header values remain a sparse
overlay on the active indexed CSV. A confirmed Split, Combine, Move Selected
Columns, Delete Selected Columns, Delete Selected Rows, or Replace All does not
stay as a lazy operation. A bounded worker materializes the operation and sparse
overlay into a private working CSV, then Quarry reopens that file as the ordinary
indexed document. Split first makes a cancellable analysis pass that retains
only the current bounded record and width counters.
The materialization pass retains a fixed read chunk, one bounded decoded
record, and its output fields. Replace All additionally retains only its two
literal inputs and a replacement counter, not every match.
RAM therefore grows with user changes, schema width, and record size rather
than file size.

The materialized working copy necessarily uses disk proportional to the current
document. Quarry retains the current private CSV and may retain one adjacent
private CSV for one-level structural Undo or Redo. Its private directory is
owner-only, and each materialized CSV is created with owner-only permissions.
Quarry removes private working files on Discard, successful publication,
document replacement, and shutdown.

## Concurrency
Rust workers currently handle indexing, overlay-aware literal search,
filtering, filtered viewport reads, filtered export, Split analysis, structural
materialization, external merge sorting, Replace All, and edited Save and Save
As. Each long operation publishes progress and supports cancellation.
Jobs normally join before they are dropped. Rapid filtered navigation cancels
obsolete reads, keeps only the newest pending window, and joins a cancelled read
after it finishes. Filter resets and document lifecycle changes detach an active
read-only viewport worker so the render thread never waits for cleanup; each
worker owns its resources and exits at its next cancellation check. An active
filtered export, structural materialization, Save, or Save As blocks document
replacement until cancellation finishes. App shutdown joins active output
workers so temporary-output cleanup is guaranteed.

## Search and filtering

Find/Replace, Filters, and Text sorting each carry an independent case-sensitivity
setting with their operation input. The unchecked default folds ASCII letters
for comparison while leaving all other bytes unchanged. **Match case** selects
raw byte comparison. This preserves byte offsets and invalid UTF-8 handling
without claiming locale-aware or full Unicode case folding.

Literal Find Next uses the Find/Replace setting in a cancellable core worker
after structural indexing. It
starts at the nearest row checkpoint, scans fixed 1 MiB chunks with the shared
delimited-record scanner, parses one bounded record at a time, substitutes any
sparse edit for the corresponding existing data cell, and retains only the
first decoded-cell match. Edits aimed outside existing data cells are ignored.
The job publishes byte/row progress and joins its worker on wait or drop. Memory
therefore depends on the query, fixed chunk, 64 MiB maximum record and decoded
fields, sparse cell overlay, one match, and bounded structural index, not on
file size or match count.

Find Previous does not start a reverse file scan. The desktop retains matches
already visited for the current query and case setting, then moves backward or
forward through that history until Find Next must resume the bounded worker.
The history grows with explicit user navigation, not the file's total match
count.

**Replace in Cell** operates only on the revealed current Find match. It
replaces every non-overlapping literal occurrence in that cell's effective
value under the same Find/Replace case setting, records the result in the
existing sparse edit map, and starts the next search. **Replace All** also uses
that setting, applies sparse data-cell edits first, skips the header, and
streams every data record through the existing private rewrite worker. A
successful run publishes and reindexes a private working CSV, then participates
in the existing one-level change history. No match, cancellation, record-limit
failure, or source conflict publishes a result. Exact core and desktop
regressions cover these semantics and the accessible controls.

Filtering still scans the active indexed CSV without the sparse data-cell
overlay, so the viewer requires those edits to be saved or discarded before a
new filter begins. An active filter must be cleared before editing or using
Find/Replace. Overlay-aware filtering remains deferred.

A `FilterQuery` owns the Filters tool's case setting and one or more
`FilterPredicate` values. Each predicate stores a source column, a contains,
equality, or inequality operator, and its literal value. Equals and Contains
predicates within one source column are alternatives. Does not equal predicates
within that column all apply, and every filtered source column must match. The
scanner parses each bounded record once, then evaluates the grouped predicates
using the query's ASCII-folded or exact comparison mode. A missing filtered
column rejects that row. `FilterQuery::single` keeps single-predicate callers
compatible with the same path. Filter to This Value and Filter Out This Value
construct their query with the current Filters setting.

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
Filtered export scans the active indexed CSV once with the same decoded-cell
predicate semantics as filtered navigation, but copies each matching raw
record to a buffered temporary file. This preserves the active header,
delimiter, quoting,
line endings, and multiline records byte for byte without retaining matching
rows. The worker publishes scanned bytes, parsed records, written rows, written
bytes, elapsed time, and cancellation state.

The temporary file is created beside the destination. A successful worker
flushes and syncs it before publishing the destination without overwriting an
existing path. Cancellation or failure removes the temporary file and never
publishes the destination. The source path itself is rejected as a destination.

## Sorting

Quarry sorts data rows by exactly one selected numbered column using stable,
ASCII case-insensitive text ordering by default. The Sort tool's **Match case**
setting switches key comparison to raw bytes. **Number** instead builds exact,
canonical decimal byte keys once per row. The key encodes sign, decimal order,
and significant digits so the same lexicographic run sort and merge compare
numbers without floating-point rounding or expanding scientific exponents.
Equivalent values share a key, including signed zero. Numeric keys add at most
10 bytes to the decoded field. The parser accepts signed dot decimals and
scientific notation with exponents from -1,000,000 to 1,000,000, trims ASCII
whitespace, and reports invalid nonblank values with their data row and column.
The header remains fixed, missing ragged fields compare as empty values, and
keys equal under the selected mode keep their current row order. Blank numeric
keys sort before numbers ascending and after numbers descending.

Character count and Word count validate UTF-8 and encode their counts as
fixed-width big-endian keys. Characters are Unicode scalar values; words use
Unicode whitespace boundaries. Shuffle hashes a seed and each current data-row
ordinal into a fixed-width key using the standard library's DefaultHasher.
The desktop obtains a fresh seed for each operation from RandomState; the
validation CLI can supply a seed to repeat a shuffle within the same build.
Reverse keys use the complemented data-row ordinal. These two modes ignore
column values and only accept ascending key order internally, so the ordinary
merge machinery shuffles or reverses whole rows while keeping the header fixed.

The core worker generates 16 MiB in-memory runs, spills owner-only framed run
files, and uses multipass merging into a private sorted working CSV. Merge
fan-in is capped at 32 and derived from the observed maximum key width while
reserving one pending key and one pending record within a 256 MiB payload
budget plus 30 bytes for numeric key overhead. Fan-in can fall to two for
very wide keys.
Heap entries retain keys and record lengths; only the selected record body is
loaded during each merge step.
The worker applies sparse edits before key comparison and output, keeps a UTF-8
BOM at the file boundary, preserves the header, and treats an absent ragged key
as empty. Before publication, a bounded dual fingerprint verifies the effective
record multiset and exact adjacent-key comparisons verify increasing source
ordinals for every stable tie.

The desktop waits for structural indexing to finish so the data-row count is
known. It combines the current file size, a two-byte-per-row fidelity cushion,
and conservative upper bounds for committed and active sparse edits. The
allowance is four times the effective byte bound plus 68 bytes per data row.
This covers two generations of framed runs, duplicated keys (including numeric
encoding overhead), and the guarded output. Scan progress switches to an active
merge phase instead of displaying 100 percent before the worker is done.

Run directories are owner-only, run files and the working CSV are owner-only,
and guarded publication checks the source before exposing the result. The
worker polls cancellation while scanning, seeding and merging runs, and before
final flush and sync. Cancellation, failure, or a source conflict removes every
unpublished run and staging file. Reopening the completed CSV lets navigation,
search, filtering, Save, Save As, Discard, and one-level Undo/Redo continue
through existing physical-row paths. A separate lazy row-order index remains
deferred until a later feature proves it is needed.

The deterministic [Phase 6A release validation](benchmarks/2026-08-21-stable-text-sort.md)
measured 16.88 MiB and 17.55 MiB peak process RSS for the 1 GB and 12 GB sorts.
Measured peak temporary disk was 2.12 GiB and 24.65 GiB, both below the
conservative preflight estimate. Complete order and preservation scans passed,
the prepublication multiset and stable-tie checks passed, source hashes remained
unchanged, and both cancellation runs finished within 4 ms without leaving a
destination or temporary run.

The [numeric validation](benchmarks/2026-09-04-numeric-sort.md) and
[additional-mode validation](benchmarks/2026-09-04-additional-sort-modes.md)
cover the newer modes on a deterministic 1 GB workload, including complete
order and row-preservation checks, cancellation, and bounded memory and disk.
They do not extend the older 12 GB and 50 GB Text results to the new modes.

## Document editing and persistence

Editing occurs directly in the grid. The last opened or saved file remains
immutable until Save. Sparse value changes use stable row and column identities
within the active indexed CSV. Cancelling an inline edit does not change
document state, and restoring the current underlying value removes that sparse
entry. A confirmed structural operation consumes the active CSV plus those
sparse edits into a new private working CSV. Quarry then reopens the result,
rebuilds offset-dependent indexes, clears the absorbed sparse overlay, and uses
the new schema as the ordinary editable document. View-only hide and reorder
actions do not alter that document schema. Explicit Move and Delete Selected
Columns actions do.

The first data-cell slice edits only existing valid UTF-8 fields. It accepts
multiline input, but does not create a missing field in a ragged row or replace
invalid UTF-8 with lossy text. The numbered row gutter stores normal, range, and
additive selection as compact physical-record ranges. **Delete Selected Rows**
streams those ranges through the private working-copy path, preserves the
header and unselected records, applies sparse edits to retained records, and
keeps Save, Save As, Discard, Undo, and Redo behavior. Filtering clears row
selection, and row selection plus deletion remain unavailable while filtered.
Row insertion remains a separate later feature.

A document is dirty while effective cell or header edits exist, or while its
active CSV is a materialized working copy that differs from the last opened or
saved file. Open, reopen, format changes, and application close must not
silently discard dirty state. Every write path must consume the complete
working copy or remain unavailable while the document is dirty. One-level
structural Undo and Redo move between adjacent indexed CSV versions and reopen
the chosen version as the current indexed grid. Discard removes all sparse edits
and private working copies, then restores the last opened or saved file.

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

The rewrite scans the active CSV once with the same quote-aware scanner used by
the other streaming workers and applies any current sparse value edits. It can
copy records without sparse edits byte for byte. Edited records are serialized
with the document delimiter plus that record's original CRLF, LF, or absent
final line ending. An edit targeting a missing row or column, a parse failure,
or serialized output above the limit fails before publication. Save targets the
logical current file only after validating both the original source stamp and
the active working snapshot stamp. Save As publishes a new file and leaves the
previous logical source unchanged.

## Column transformations

The desktop calls the multi-column operation **Combine Columns…**, while
`quarry-core` represents that operation as `ColumnTransformation::Join`. This
section uses Combine for the user-facing operation and Join only for the engine
variant.

The desktop's **Move Selected Columns…** and **Delete Selected Columns** actions
both map to `ColumnTransformation::Arrange`. The variant carries the known
source width and the unique source-column indexes to retain in final order.
Move emits every known column in the requested order; Delete omits the selected
known columns. Fields beyond the known source width are appended unchanged so
later ragged data is never discarded because the viewer has not discovered it.

The numbered grid headers own structural selection. The context menu for one
selected column offers **Split Columns…**; selecting at least two columns also
offers **Combine Columns…**. Each command opens a compact modal prefilled from
the current selection. Split requires a non-empty literal separator, while
Combine accepts an optional literal separator. The dialog asks only for the
separator and confirmation. Cancel closes it without changing the document.
The same context menu offers **Move Selected Columns…** and **Delete Selected
Columns**. Move opens a compact modal with a labelled one-based destination
field and Move/Cancel buttons. Delete starts directly after checking that at
least one known column will remain.

Split first scans the current data plus sparse edits to derive the maximum
number of separated parts, then replaces the selected column using that derived
schema. The original header stays on the first result, additional headers are
blank and editable, and following columns move to their new one-based document
positions. Rows with fewer parts receive empty fields. If no value contains the
separator, Split reports that nothing can be split and does not materialize a
new generation. Combine reads the selected columns in document order, combines
them with the separator, inserts the result at the leftmost selected position,
and removes the selected originals. The result keeps the leftmost selected
current header. Missing selected fields in ragged rows are empty, and the
resulting header remains editable.

Move removes the selected known columns, keeps them in ascending current
document order as one block, and inserts that block at the requested first
output position. Every unselected known column retains its relative order. The
destination range is 1 through `known columns - selected columns + 1`. An
identity move returns before reserving a working generation so Undo/Redo
history is unchanged. Delete removes only the explicitly selected known
columns and selects the nearest survivor after materialization. Hidden state
and view order are never consulted by either operation.

Confirmation starts the bounded background operation. Split performs its
analysis pass, then Split, Combine, Move Selected Columns, or Delete Selected
Columns streams the current document into a newly reserved private working CSV.
Only that one operation is evaluated during the stream. After the worker
succeeds, Quarry opens and indexes the result as the normal editable grid. The
user can edit the result or apply another structural command, which repeats the
same process from the current working CSV. Every shown working-document column
is horizontally reachable; the viewport paints at most 64 at a time, and the
desktop uses the core
65,536-column structural trust limit.

The grid exposes column selection state and context actions through AccessKit.
The modals bind visible labels to their input fields, provide named confirmation
and Cancel buttons, announce background status changes, and expose the cancel
action while a worker is active. Reopening a materialized result preserves an
affected surviving column selection in the ordinary grid.

Save streams the current working CSV plus any newer sparse edits and atomically
replaces the logical current regular file. Save As publishes and opens a new
file without changing the previous source. Discard restores the last opened or
saved file. Structural column operations cannot start from an active filtered
view.

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

## macOS packaging and application identity

The internal `quarry-egui` release binary is packaged as the customer-facing
`/Applications/Quarry.app`. The stable bundle identifier is
`io.github.danchamorro.quarry`, and the bundle executable is `Quarry`.
`packaging/macos/Info.plist`, `assets/quarry-logo-v3.png`, and
`scripts/macos-app.sh` are the versioned packaging inputs.

The package command performs a locked release build, injects the Cargo version,
full-history commit-count build number, full commit, source status, and
architecture, converts the checked-in logo to `Quarry.icns`, applies an ad-hoc
signature, and strictly verifies the candidate. The CI macOS job runs the same
command. Matching payload hashes are expected only from the same checkout,
pinned Rust 1.88.0 toolchain, and macOS SDK; the workflow does not pin the
runner-provided SDK. The package and install commands share a per-user native
file lock, and the packager rejects a checkout that changes while Cargo is
building.

The egui app holds a shared per-user installation lock for its process lifetime,
while installation holds the exclusive form through replacement and rollback.
This prevents a participating app from starting during a bundle swap. Process
name checks remain as a migration guard for legacy builds that predate the
cooperative lock.

Installation copies the verified candidate beside the final destination,
verifies it again, then swaps it into the canonical path. When a prior installed
app exists, it remains available for in-operation restoration and a verified
zip archive is retained under `~/Library/Application Support/Quarry/Backups`.
The installed plist and signed executable must exactly match the candidate.
After success, the installer removes any legacy app and staged candidate so
LaunchServices has one active application with the canonical identity.

Build artifacts under `target` are not acceptance-test applications. Packaged
interaction checks launch `/Applications/Quarry.app`. Developer ID signing,
hardened runtime, notarization, stapling, and Gatekeeper assessment remain a
later distribution boundary.

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

Move Selected Columns and Delete Selected Columns use the same bounded
structural materialization worker measured for Split and Join. Replace All
reuses its private rewrite, progress, cancellation, guarded-publication, and
working-copy paths. Exact core and
desktop regressions validate Arrange ordering and deletion plus replacement
overlay precedence, non-overlapping matches, no-match behavior, record limits,
accessibility, Undo, source preservation, cancellation, and temporary-file
cleanup. The [12 GB Replace All benchmark](benchmarks/2026-08-22-12gb-replace-all.md)
and [50 GB capability suite](benchmarks/2026-08-22-50gb-capability-suite.md)
measure the production Replace All path directly. Separate large-file Move
Selected Columns and Delete Selected Columns timings are not claimed because
those operations add no new worker-path evidence.

Delete Selected Rows uses the same guarded private rewrite boundary with direct
record skipping. Its [Phase 8A release validation](benchmarks/2026-09-04-delete-selected-rows.md)
records complete 1 GB and 12 GB runs below 4 MiB peak RSS, unchanged source
hashes, and successful destination reopen checks.

## Architecture rule
**If a feature only works because the entire file fits in RAM, it is not a finished Quarry feature.**
