# Quarry Roadmap

The roadmap is ordered by technical risk rather than feature excitement.

## Current progress: 2026-08-16

| Phase | Status | Evidence |
|---|---|---|
| Phase 0 — Foundation | Complete | Rust workspace, CI, deterministic generator, ADR, licensing, and the [12 GB benchmark](benchmarks/2026-08-14-large-file.md) |
| Phase 1 — Prove the core | Complete | Progressive open, correct parsing, bounded indexing, live navigation, cancellation, and the measured [no-cache decision](adr/0002-defer-viewport-cache.md) |
| Phase 2 — UI bake-off | Complete | The [egui](benchmarks/2026-08-14-egui-spike.md) and [AppKit](benchmarks/2026-08-14-appkit-spike.md) candidates were measured; [ADR 0003](adr/0003-select-egui-ui.md) selects egui |
| Phase 3: Viewer alpha | Complete | Continuous bounded scrolling is measured on the [12 GB reference file](benchmarks/2026-08-15-continuous-scroll.md); native opening and format controls are covered by the [viewer file-controls validation](benchmarks/2026-08-15-viewer-file-controls.md); bounded Find Next is covered by the [streaming-search benchmark](benchmarks/2026-08-15-streaming-search.md); cell and row copying are covered by the [bounded-copy validation](benchmarks/2026-08-16-bounded-copy.md); direct column access is covered by the [column-controls validation](benchmarks/2026-08-16-column-controls.md); the maximized layout is covered by the [row-density validation](benchmarks/2026-08-16-row-density.md) |
| Phase 4: Filters and export | Complete | Bounded single and multiple AND-predicate filtering is tracked in the [streaming-filter](benchmarks/2026-08-16-streaming-filter.md) and [multiple-predicate filter](benchmarks/2026-08-16-multiple-predicate-filter.md) validations; safe streaming export passed its [1 GB and 12 GB validation](benchmarks/2026-08-16-filtered-export.md) |

### Phase 1 checklist

- [x] Return the first viewport before full-file indexing.
- [x] Parse quoted delimiters, escaped quotes, CRLF, and embedded newlines.
- [x] Keep structural-index memory bounded with adaptive checkpoints.
- [x] Navigate a row range from a completed index.
- [x] Navigate a row range as soon as the live index reaches it.
- [x] Cancel and join background indexing promptly.
- [x] Report first-row time, throughput, progress, memory, and index size.
- [x] Measure repeated nearby and random viewport reads, then add a bounded
  cache only if the measurements justify it.

**Phase 1 exit met:** the 12 GB reference file opens and navigates with bounded
memory, and warm random 100-row reads complete in 1.843 ms at p95 without an
application cache.

### Phase 2 checklist

- [x] Build an egui spike on the existing engine.
- [x] Keep each UI request bounded to 100 rows.
- [x] Wire open, previous/next, jump, progress, cancellation, and keyboard paging.
- [x] Expose controls, headers, and visible cells through macOS accessibility.
- [x] Collect repeatable bounded-navigation and sustained-scroll measurements.
- [x] Build the native AppKit comparator.
- [x] Document the production UI decision in an ADR.

**Phase 2 exit met:** both candidates open the 12 GB reference file within the
memory target and remain responsive under bounded navigation and viewport
scrolling. egui is selected for Phase 3; continuous file-level scroll frame
pacing becomes measurable with the viewer-alpha grid.

### Phase 3 checklist

- [x] Replace visible previous/next paging with continuous row scrolling.
- [x] Map one scrollbar over the indexed row range without file-sized pixel
  content.
- [x] Keep the header visible and retain horizontal scrolling, row jump, and
  Page Up/Page Down navigation.
- [x] Materialize only the visible rows plus a two-row overscan on each side.
- [x] Cover progressive refill, wheel and page movement, empty files, first,
  midpoint, final, 117-million-row, overflow, and exact-final-record behavior
  with regression tests.
- [x] Record 12 GB continuous-scroll latency, memory, frame-pacing, and final-row
  correctness evidence.
- [x] Open local files through a native picker, typed or CLI paths, and one-file
  drag and drop without replacing a valid document on picker cancellation or an
  open or index-start failure.
- [x] Apply Auto, comma, tab, pipe, and semicolon delimiter modes plus Auto,
  first-row, and no-header modes to the current file only through an explicit
  reopen.
- [x] Start a replacement indexer successfully, then cancel and join the prior
  worker before installing it as the current document.
- [x] Benchmark live-index snapshot latency and indexing throughput together,
  then reduce the default chunk from 8 MiB to 1 MiB based on the measured
  [live-index latency result](benchmarks/2026-08-15-live-index-latency.md).
- [x] Expose file size, column count, delimiter/header mode, indexed rows,
  first-row time, and viewport latency in the viewer status area.
- [x] Add literal, case-sensitive Find Next over decoded cells with background
  progress, cancellation, same-query resume, and direct row-and-column reveal.
- [x] Record quoted/multiline correctness on a deterministic fixture, plus
  complete-scan throughput, cancellation, and measured peak RSS on the
  deterministic 1 GB and 12 GB datasets in the
  [streaming-search benchmark](benchmarks/2026-08-15-streaming-search.md).
- [x] Copy one selected visible cell or full parsed row through a visible action
  and Command+C, preserving cell text or serializing a row as UTF-8 TSV within
  a 64 MiB limit.
- [x] Cover multiline and quoted fields, tabs, invalid UTF-8, the output limit,
  text-input shortcut focus, viewport selection lifecycle, and stable
  accessibility identity with regressions, then validate the macOS clipboard
  at data row 100,000,000 on the
  [12 GB reference file](benchmarks/2026-08-16-bounded-copy.md).
- [x] Add direct manual access to every known column plus view-only hide/show
  and arbitrary reorder controls inside the Columns manager, while preserving
  resize-only grid headers with stable one-based file-column numbers, bounded
  32-column rendering, search reveal, reset behavior, and accessibility.
- [x] Cover the column controls with deterministic wide-file regressions and a
  [12 GB viewer check](benchmarks/2026-08-16-column-controls.md).
- [x] Make the default grid dynamically fit compact rows to the available
  height. The maximized reference window keeps at least 40 data rows visible
  instead of 23, using EmEditor only as a density reference while preserving
  legibility, accessibility, and bounded virtualization in the
  [row-density validation](benchmarks/2026-08-16-row-density.md).

**Phase 3 exit met:** the viewer combines bounded continuous navigation, native
opening, format controls, literal search, copy, column controls, diagnostics,
and an adaptive grid that keeps at least 40 rows visible in the maximized
reference window.
Phase 4 filtering and streaming filtered export are complete. Phase 5 column
transformations are next. Cosmetic redesign remains outside the current scope.

## Phase 0 — Foundation
Rust workspace, CI, lint/test policy, deterministic large-file generator, benchmark harness, 1 GB/10 GB profiles, CLI experiments, ADR process, and license decision.

**Exit:** reproducible benchmark results on a known machine.

## Phase 1 — Prove the core
Open/map huge files, sample format, parse first viewport, background structural indexing, measured cache decision, row-range navigation, cancellation, and diagnostics.

**Exit:** a 10 GB CSV can be opened and navigated through the engine with bounded memory.

## Phase 2 — UI bake-off
Build minimal UI prototypes against the same Rust engine. Compare frame time, CPU, memory/allocation behavior, keyboard navigation, accessibility, startup, and integration complexity.

**Exit:** documented UI architecture decision.

## Phase 3 — Viewer alpha
Virtualized grid, drag/drop/open, delimiter/header controls, view-only column
controls, jump-to-row, streaming search, copy, status, diagnostics.

**Exit:** Quarry is genuinely useful for inspecting huge files.

## Phase 4 — Filters and export
Contains/equality filters, multiple AND predicates, incremental results, filtered navigation, streaming export, progress/cancellation.

### Phase 4 checklist

- [x] Add literal, case-sensitive contains and equality predicates over one
  selected source column.
- [x] Build an adaptive match index with a fixed memory budget, background
  progress, prompt cancellation, and joined worker lifecycle.
- [x] Serve bounded filtered row ranges from the nearest match checkpoint in a
  cancellable background worker and navigate them through the egui viewer.
- [x] Cover selected-column isolation, decoded quotes, multiline fields, exact
  match counts, adaptive compaction, range reads, and cancellation with
  deterministic regressions.
- [x] Record complete-scan throughput, cancellation, bounded index memory, and
  peak RSS on the deterministic 1 GB and 12 GB datasets.
- [x] Combine multiple predicates with AND semantics, parse each row once, keep
  the adaptive index bounded, preserve single-predicate compatibility, and
  record 1 GB and 12 GB evidence in the
  [multiple-predicate filter validation](benchmarks/2026-08-16-multiple-predicate-filter.md).
- [x] Stream filtered rows to a new output file with progress, cancellation,
  source-file safety, exact output validation, and bounded 1 GB and 12 GB RSS
  evidence in the
  [filtered-export validation](benchmarks/2026-08-16-filtered-export.md).

**Next:** begin Phase 5 with a bounded transformation preview before adding
saved transformed output.

**Phase 4 exit met:** multiple predicates and filtered export remain practical
on the 12 GB reference file while memory stays bounded and the source stays
unchanged.

## Phase 5 — Column transformations
Split, join, rename, reorder/drop in saved output, find/replace, preview,
reusable transformation pipeline.

Add **Save As** modes for an unchanged source-order copy and a transformed
copy. A transformed copy writes columns in the current arranged order. Hiding
a column does not remove it from saved output unless the user explicitly
chooses to exclude it; the source file remains unchanged.

## Phase 6 — Sorting
Type semantics, external run generation, spill management, merge, stable row-order abstraction, disk-space estimation.

## Phase 7 — Hardening
Persistent indexes, invalidation, malformed-data UX, encoding strategy,
packaging/signing/notarization, accessibility audit, controlled cold-cache
performance runs, benchmark dashboard.

## Later possibilities
Cross-platform front ends, plugin/API surface, schema inference, SQL-like querying, compressed files, CLI recipes, multi-file operations.

## Performance ladder
1. 1 GB — development baseline
2. 10 GB — first product promise
3. 25 GB — stress
4. 50 GB — serious stress
5. File larger than available RAM — architectural proof
