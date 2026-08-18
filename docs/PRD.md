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

Phase 5 has delivered direct in-grid header rename, unsaved document state,
atomic Save with metadata-based conflict detection, and no-clobber Save As.
Direct cell editing is next. Disk-aware sorting remains
a candidate if it does not delay the core milestone.

Not required: general-purpose text-editor behavior, formulas, charts, database
connectivity, plugins, cloud sync, collaboration, direct in-place byte
mutation, or EmEditor feature parity.

## Version 0.2: direct editing and transformations

Edit headers and cells directly in the grid. The first slice renames an
existing header and records the change as sparse unsaved document state keyed
by stable source-column identity, with Save and Save As. Later slices add direct
cell editing, split/join, rename/reorder/drop, find/replace, transformed
filtered output, and safe streaming full-file output.

Save As writes a selected destination, leaves the previous source unchanged,
and makes the new file current only after success. Save writes through a
same-directory temporary file, preserves standard file permissions, rejects
the Save when a metadata-visible source change is found at startup or
immediately before replacement, flushes and syncs, then atomically replaces the
current regular file. Final-path symbolic links require Save As. Neither
operation mutates records in place.

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
- Responsive UI during indexing, search, filtering, filtered navigation, and
  export.
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
