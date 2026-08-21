# Quarry Product Requirements Document

**Version:** 0.1 Draft
**Platform:** macOS first
**Core engine:** Rust
**Model:** Open source

## Product vision
Quarry is a performance-first desktop application for people who work with delimited text files too large for conventional editors and spreadsheets.

### North star
> A user should be able to open a 10 GB CSV on a Mac, see useful data within seconds, and begin navigating it without waiting for the entire file to load into memory.

Longer term, Quarry should make files substantially larger than physical RAM practical to inspect and transform.

## Problem
Data professionals regularly receive multi-gigabyte CSV, TSV, and pipe-delimited files. Common tools may consume enormous memory, freeze, impose limits, or require importing the data elsewhere before inspection. Quarry fills that gap on macOS.

## Target users
Data operations professionals, data engineers, ETL developers, analysts, database professionals, integration engineers, and technical users receiving massive exports.

## Product principles
1. Performance is a feature.
2. File size must not imply equivalent RAM usage.
3. Useful data appears before complete indexing.
4. Background work must not freeze the interface.
5. Every major feature needs a credible strategy for files larger than RAM.
6. Benchmark continuously.
7. The Rust engine owns data-intensive work; the UI remains thin.
8. Do not chase feature parity with EmEditor or another editor.
9. Correctness and data integrity outrank clever optimization.
10. Fail explicitly rather than silently corrupt data.

## Version 0.1 — read-only exploration
Required: local file opening; delimiter detection/override; header selection;
virtualized grid; navigation; jump-to-row; view-only column
resize/hide/show/reorder; streaming search; bounded copy of one cell or row;
one or more AND-combined literal filter predicates; background structural
indexing; safe streaming filtered export to a new file;
progress/cancellation; parsing metadata; diagnostics and benchmarks.

Column controls operate on stable source-column identities. Header columns are
available immediately, while extra fields in ragged rows become available when
the viewer encounters them.

Not required: general-purpose text-editor behavior, formulas, charts, database
connectivity, plugins, cloud sync, collaboration, direct in-place byte
mutation, or EmEditor feature parity.

## Version 0.2: direct editing and transformations

Phase 5 has delivered direct in-grid header and existing data-cell editing,
sparse unsaved document state, atomic Save with metadata-based conflict
detection, no-clobber Save As, and repeatable Split and Combine edits applied
from selected grid columns. Each confirmed operation materializes a private CSV
and reopens it as the ordinary editable working copy. Explicit selected-column
Move and Delete use that same workflow while the Columns manager remains
view-only. Overlay-aware Find Next, Replace in Cell, and bounded Replace All
complete Phase 5. Phase 6A adds stable single-column text sorting through a
disk-aware external merge worker. The desktop, core, and validation CLI passed
the deterministic 1 GB and 12 GB release validation.

Edit headers and cells directly in the grid. Existing UTF-8 data cells support
multiline input and use sparse unsaved document state keyed by stable physical
record row and column identity in the current indexed document. Missing fields
in ragged rows and invalid UTF-8 cells fail explicitly rather than creating or
corrupting data.
Find Next scans the active indexed CSV while treating each sparse data-cell edit
as that cell's effective value. Replace in Cell changes every non-overlapping
literal occurrence in the current matched cell and continues searching.
Replace All applies sparse cell edits first, skips the header, and streams every
data row through the bounded private rewrite worker. A successful replacement
reopens as the ordinary unsaved working document; no match, cancellation, or
failure changes nothing. Filters still require unsaved data-cell edits to be
saved or discarded. After a materialized operation is reindexed, its working
CSV becomes the ordinary searchable document. Split replaces one selected
column with the fields found by a
non-empty literal separator. Quarry derives the resulting width from the
current data plus sparse edits, keeps the original header on the first result,
and creates blank editable headers for additional results. If the separator is
absent from the selected column, the operation reports that no split is
possible and leaves the document unchanged. Combine merges at least two
selected columns in document order with a literal separator that may be empty,
inserts the result at the leftmost selected position, keeps that position's
current header, and removes the selected originals. Both operations renumber
the changed document schema.

Move takes the selected known columns in ascending current-document order and
places them as one block at a one-based destination after removing them from
the current order. Unselected columns retain their order, and an identity move
is a true no-op that preserves Undo/Redo history. Delete removes only the
explicitly selected known columns, cannot remove every known column, and
selects the nearest surviving column. Hidden or view-reordered columns are not
implicit output instructions. Undiscovered trailing fields in ragged rows stay
after the arranged known columns so a partial view cannot silently discard
data.

Users start **Split Columns…** or **Combine Columns…** from the context menu of
the numbered column headers. A compact modal uses the current selection and
asks only for the operation's separator. **OK** starts a bounded background
stream into a private working CSV, and **Cancel** does nothing. Quarry opens and
indexes the completed working file as the normal editable grid. Users may edit
the resulting cells and headers or repeat any structural column command before
writing a file. One-level structural Undo and Redo move between adjacent
document versions. Quarry displays at most 32 document columns per viewport and
applies the core 65,536-column structural safety limit. Bounded display work does
not otherwise limit Save or Save As.

Users start **Move Selected Columns…** or **Delete Selected Columns** from the
same numbered-header context menu. Move uses a compact modal with the selected
columns and a labelled one-based destination field, plus Move and Cancel.
Delete starts immediately after validating that at least one known column will
survive. Both operations stream into a private working CSV and return to the
ordinary editable grid.

Numbered column headers expose their selected state and **Split Columns…**,
**Combine Columns…**, **Move Selected Columns…**, and **Delete Selected
Columns** context actions to accessibility clients. With exactly one selected
column, **Sort Rows…** opens an accessible ascending/descending text-sort
dialog with explicit semantics, a conservative temporary-disk allowance, Sort,
and Cancel. Transformation dialog
headings, selected-column summaries, labelled fields, confirmation and Cancel
buttons, background status, and cancellation actions have stable accessible
names and remain keyboard operable. After materialization, focus returns to the
ordinary grid and an affected surviving working column remains selected.

Save As writes a selected destination, leaves the previous source unchanged,
and makes the new file current only after success. Save writes through a
same-directory temporary file, preserves standard file permissions, rejects
the Save when a metadata-visible source change is found at startup or
immediately before replacement, flushes and syncs, then atomically replaces the
current regular file. Final-path symbolic links require Save As. Neither
operation mutates records in place. Discard Changes removes every sparse edit
and private working version, then restores the last opened or saved file.

Save and Save As scan the current indexed CSV, copy unedited records byte for
byte, and serialize only records with newer sparse edits using the document
delimiter and original line ending. For a fixed edit set, RAM depends on the
bounded scanner record and sparse overlay rather than file size. A structural
operation uses disk proportional to the current CSV, plus one prior private CSV
when retained for one-level Undo. Overlay memory may grow with the number and
size of edits.

Replace All reuses that private rewrite worker, progress, cancellation,
guarded publication, and working-copy history. It retains a fixed read chunk,
one bounded decoded record and its output fields, plus the sparse overlay and
literal inputs rather than a file-sized replacement list. Existing 1 GB and
12 GB persistence measurements cover the shared worker; exact core and desktop
regressions own Replace All semantics without claiming separate large-file
Replace All timings.

## Version 0.3: sorting (complete)

Phase 6A sorts data rows by exactly one selected
numbered column as case-sensitive text, ascending or descending. The header
stays fixed, missing ragged fields compare as empty values, and equal keys keep
their current row order. The disk-aware external merge worker builds bounded
runs, caps merge payload memory, reports a conservative temporary-space
allowance before starting, and publishes only a complete private sorted CSV.
The desktop immediately reopens that CSV as Modified and reuses Save, Save As,
Discard, Undo, and Redo. Cancellation, failure, or a source conflict leaves the
current document unchanged. Exact regressions and deterministic 1 GB and 12 GB
release runs pass with peak RSS below 20 MiB, measured temporary storage below
the conservative estimate, bounded effective-record multiset evidence, exact
stable-tie source-ordinal checks, unchanged source hashes, and prompt
cancellation without leftovers. Numeric, date, locale-aware, case-insensitive,
and multi-column sorting remain later slices.

## Progressive opening
1. Open the file and sample a bounded region.
2. Detect likely encoding/delimiter/quote settings.
3. Parse enough records for the first viewport.
4. Display them immediately.
5. Continue structural indexing in background workers.
6. Enable index-dependent navigation and search as structural indexes become available.
7. Start filtering directly from the source file as an independent, cancellable scan.

## Performance requirements
Reference benchmark: representative **10 GB CSV** on modern Apple Silicon with SSD storage.

Initial targets:
- First useful rows: under 3 seconds target.
- Initial viewing memory: under 500 MB target.
- No RAM growth proportional to file size during read-only viewing.
- Responsive UI during indexing, search, filtering, filtered navigation,
  export, editing, Find/Replace, Split, Combine, Move, and Delete.
- Streaming Save and Save As with memory bounded by scanner limits and the
  sparse edit set rather than source-file size.
- Long operations cancellable.
- Smooth interactive scrolling.

Every benchmark must identify hardware, OS, dataset, build mode, and cache state. Cold-cache performance must not be hidden behind warm-cache numbers.

## Reliability and safety

Opening and editing never modify source bytes. Committed edits remain unsaved
document state until Save or Save As succeeds. Both operations stream through
temporary output and flush and sync it before publication. Cancellation or
failure before publication removes Quarry's temporary output without Quarry
replacing the current file or clobbering an existing destination. Opening,
reopening, or closing a changed document requires an explicit save or discard
decision.

Replace All follows the same rule: it publishes only a complete private working
copy, leaves the opened or saved source unchanged, and removes unpublished
output after no match, cancellation, or failure.

## Accessibility
The selected UI must preserve keyboard navigation, VoiceOver, focus behavior,
native text behavior, and high-DPI rendering as features are added.

## Alpha success criterion
A 10 GB CSV is repeatedly practical to open and navigate on supported Macs while memory stays bounded and the UI remains responsive.

## Positioning
> **Quarry — the open-source, performance-first data file editor for macOS.**

Another editor's trademark should not be Quarry's official tagline.

## Open questions
Minimum macOS version; encoding breadth; persistent index format/invalidation.
[ADR 0003](adr/0003-select-egui-ui.md) selects the UI framework.

The initial engine ships with a benchmark-oriented CLI and is dual-licensed
under MIT or Apache-2.0.
