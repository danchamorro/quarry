# Quarry Roadmap

The roadmap is ordered by technical risk rather than feature excitement.

## Current progress: 2026-08-23

| Phase | Status | Evidence |
|---|---|---|
| Phase 0 — Foundation | Complete | Rust workspace, CI, deterministic generator, ADR, licensing, and the [12 GB benchmark](benchmarks/2026-08-14-large-file.md) |
| Phase 1 — Prove the core | Complete | Progressive open, correct parsing, bounded indexing, live navigation, cancellation, and the measured [no-cache decision](adr/0002-defer-viewport-cache.md) |
| Phase 2 — UI bake-off | Complete | The [egui](benchmarks/2026-08-14-egui-spike.md) and [AppKit](benchmarks/2026-08-14-appkit-spike.md) candidates were measured; [ADR 0003](adr/0003-select-egui-ui.md) selects egui |
| Phase 3: Viewer alpha | Complete | Continuous bounded scrolling is measured on the [12 GB reference file](benchmarks/2026-08-15-continuous-scroll.md); native opening and format controls are covered by the [viewer file-controls validation](benchmarks/2026-08-15-viewer-file-controls.md); bounded Find Next is covered by the [streaming-search benchmark](benchmarks/2026-08-15-streaming-search.md); cell and row copying are covered by the [bounded-copy validation](benchmarks/2026-08-16-bounded-copy.md); direct column access is covered by the [column-controls validation](benchmarks/2026-08-16-column-controls.md); the maximized layout is covered by the [row-density validation](benchmarks/2026-08-16-row-density.md) |
| Phase 4: Filters and export | Complete | Bounded single and multiple AND-predicate filtering is tracked in the [streaming-filter](benchmarks/2026-08-16-streaming-filter.md) and [multiple-predicate filter](benchmarks/2026-08-16-multiple-predicate-filter.md) validations; safe streaming export passed its [1 GB and 12 GB validation](benchmarks/2026-08-16-filtered-export.md) |
| Phase 5: Direct editing and transformations | Complete | Inline editing, atomic Save with metadata-based conflict detection, and no-clobber Save As passed the deterministic [direct-edit validation](benchmarks/2026-08-18-direct-cell-editing.md); the Split/Join persistence engine passed the deterministic [1 GB and 12 GB transformation validation](benchmarks/2026-08-19-split-join-transformations.md); exact core and desktop regressions cover selected-column Move/Delete, overlay-aware Find Next, and Replace in Cell; production Replace All is measured directly at [12 GB](benchmarks/2026-08-22-12gb-replace-all.md) and [50 GB](benchmarks/2026-08-22-50gb-capability-suite.md) |
| Phase 6: Sorting | Complete | Stable single-column text sorting passed exact regressions plus the deterministic [1 GB and 12 GB Phase 6A validation](benchmarks/2026-08-21-stable-text-sort.md); the current engine also passed the optimized [117-million-row `FIRSTNAME` sort](benchmarks/2026-08-23-12gb-sort-performance.md), including bounded RSS, measured temporary disk, complete order and preservation scans, and cancellation cleanup |
| Phase 7: Hardening | In progress | Phase 7A packaging and installation are complete in the [packaged-app validation](benchmarks/2026-08-21-packaged-app.md); Phase 7B workflow polish and feature-gap review are underway |

Cross-phase evidence: the [50 GB capability suite](benchmarks/2026-08-22-50gb-capability-suite.md)
measures progressive open, complete indexing, navigation, Find, filtering,
filtered export, sparse Save As, Replace All, Split, Combine, and stable Sort on
a 48.25 GiB file while preserving bounded process memory and the source file.

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
  64-column rendering with horizontal access to every shown column, search
  reveal, reset behavior, and accessibility.
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
Phase 4 filtering and streaming filtered export are complete. Phase 5 has
direct in-grid header and data-cell editing, sparse streaming persistence, and
repeatable selected-column Split and Combine commands in the ordinary editable
grid. Explicit Move and Delete use the same selected numbered columns and
working-copy lifecycle while the Columns manager remains view-only. Find Next,
Replace in Cell, and bounded Replace All use effective unsaved cell values.
Cosmetic redesign remains outside the current scope.
Phase 6 is complete. Stable, selected-column text sorting works in the ordinary
editable grid and passed the deterministic 1 GB and 12 GB release gate.
Phase 7A is complete with one repeatable macOS package command, one canonical
installed application, and a clean committed release install.

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
Contains, equality, and inequality filters, multiple AND predicates, incremental results, filtered navigation, streaming export, progress/cancellation.

### Phase 4 checklist

- [x] Add literal, case-sensitive contains, equality, and inequality predicates over one
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

**Phase 4 exit met:** multiple predicates and filtered export remain practical
on the 12 GB reference file while memory stays bounded and the source stays
unchanged.

## Phase 5: Direct editing and transformations

Edit values where they appear in the grid. Keep committed edits as sparse
unsaved document state until an explicit Save or Save As. Header rename and
editing existing data cells use stable identities in the current indexed
document. Select numbered column headers, open **Split Columns…** or
**Combine Columns…** from their context menu, enter a literal separator in a
compact OK/Cancel dialog, and continue in the same editable grid.
Each confirmed operation streams the current document into a bounded private
working CSV, reopens it as the ordinary indexed document, and supports more
edits or transformations without building a file-sized in-memory model.
One-level structural Undo and Redo reopen the preceding or subsequent document
version.

**Next:** begin Phase 7B with workflow polish, refinement, and a feature-gap
review after the current interactions are coherent.

### Phase 5 checklist

- [x] Rename an existing header directly in the grid while preserving its
  stable source-column identity.
- [x] Track effective edits as unsaved document state and prevent open, reopen,
  or close from silently discarding them.
- [x] Add Save As: stream the current document to a selected path, publish only
  after a complete flush and sync, leave the original unchanged, and make the
  new path current only after success.
- [x] Add Save: stream through a same-directory temporary file, preserve
  standard file permissions, check for metadata-visible source changes when
  Save starts and immediately before replacement, and atomically replace the
  current regular file only after success. Final-path symbolic links require
  Save As.
- [x] Reopen a successful Save or Save As destination, rebuild offset-dependent
  indexes, and clear the unsaved state only after the new document is ready.
- [x] Remove Save and Save As temporary output after cancellation observed
  before publication or a write failure, without Quarry replacing the current
  file or clobbering an existing destination. For a fixed sparse edit set, keep
  memory bounded with respect to source-file size.
- [x] Edit an existing UTF-8 data cell directly in the grid by stable physical
  record row and source column. Preserve multiline input, cancel an active edit
  with Escape, reject missing ragged fields and invalid UTF-8 explicitly, and
  keep search/filter results from silently ignoring unsaved cell values.
- [x] Stream header and cell overlays through Save and Save As. Copy unedited
  records byte for byte, serialize only edited records with the document
  dialect and original line ending, and reject invalid row or column targets
  before publication.
- [x] Record exact output, source preservation, cancellation, throughput, and
  peak RSS on deterministic 1 GB and 12 GB datasets in the
  [direct-cell editing validation](benchmarks/2026-08-18-direct-cell-editing.md).
- [x] Add selected-column Split and Combine context actions without building a
  file-sized in-memory document model. Use a compact OK/Cancel dialog, derive
  Split width from the current data, materialize each confirmed operation into
  a private working CSV, reopen it in the ordinary editable grid, and allow
  repeated edits and transformations. One-level structural Undo and Redo move
  between adjacent document versions. Save and Save As publish the current
  working document, while Discard restores the last opened or saved file.
  Record exact semantic first/middle/final validation, source preservation,
  cancellation, throughput, and peak RSS for the persistence engine on
  deterministic 1 GB and 12 GB datasets in the [split/join transformation
  validation](benchmarks/2026-08-19-split-join-transformations.md).
- [x] Add explicit **Move Selected Columns…** and **Delete Selected Columns**
  actions to numbered-header context menus while keeping the Columns manager
  view-only. Move the selection as one ordered block to a one-based destination
  without changing history for an identity move. Reject deleting every known
  column, preserve undiscovered trailing ragged fields, materialize through the
  existing cancellable private working-copy path, and retain Save, Save As,
  Discard, and structural Undo/Redo behavior. Exact Arrange regressions cover
  semantics and source safety; the existing [Split/Join transformation
  validation](benchmarks/2026-08-19-split-join-transformations.md) supplies the
  shared worker's 1 GB and 12 GB bounded-memory and cancellation evidence.
- [x] Make literal Find Next overlay-aware so unsaved cell values replace their
  source values during matching. Add **Replace in Cell** for the current match
  and a bounded, cancellable **Replace All** that applies sparse edits first,
  skips the header, materializes a private working CSV, and participates in
  Save, Save As, Discard, and one-level Undo/Redo. Replace All reuses the
  private rewrite worker measured by the 1 GB and 12 GB persistence
  validations; exact core and desktop regressions cover overlay-first
  semantics, non-overlapping replacement, no-match, record limits,
  cancellation, cleanup, accessibility, and change history.

The Columns manager remains view-only and does not mark the document changed.
Move and Delete are explicit and separate from that view arrangement. Hidden
columns remain in output; to delete one, show it and select it explicitly.

**Phase 5 exit met:** direct edits, explicit output reorder/drop, overlay-aware
Find Next, Replace in Cell, bounded Replace All, and structural transformations
use workers with 1 GB and 12 GB bounded-memory evidence. Exact regressions own
the newer feature semantics, unsaved changes cannot be lost silently, and Save
and Save As never expose a partial or corrupted file.

## Phase 6 — Sorting

Phase 6 is complete. Phase 6A lets users select exactly one numbered
column and apply a stable ascending or descending, case-sensitive text sort to
data rows while keeping the header fixed. Missing ragged fields compare as
empty values, and equal keys retain their current row order. The implementation
and deterministic 1 GB and 12 GB release evidence pass.

### Phase 6A checklist

- [x] Add an accessible **Sort Rows…** action with one selected column, order,
  explicit text semantics, a temporary-disk estimate, Sort, and Cancel.
- [x] Generate bounded in-memory runs, spill owner-only framed run files, and
  merge them with bounded fan-in into a private sorted working CSV.
- [x] Apply current sparse edits before key comparison and output, then reopen
  the sorted result in the ordinary grid as Modified with existing Save, Save
  As, Discard, and one-level Undo/Redo behavior.
- [x] Preserve the source and current document on cancellation, failure, or a
  source-stamp conflict, and remove every unpublished run and staging file.
- [x] Cover stable ascending/descending order, quoted and multiline keys,
  ragged rows, sparse overlays, forced multi-run merging, cancellation, cleanup,
  accessibility, and change history with exact regressions.
- [x] Record deterministic 1 GB and 12 GB time, peak RSS, peak temporary disk,
  merge passes, exact row/header preservation, bounded record-multiset evidence,
  exact stable-tie order, source hashes, and cancellation latency before marking
  Phase 6A complete.

**Phase 6A exit:** stable text sorting immediately appears in the editable grid,
memory stays bounded with respect to file size, required temporary disk is
clear before starting, and cancellation cannot expose a partial result.

**Phase 6A exit met:** the [release validation](benchmarks/2026-08-21-stable-text-sort.md)
records complete 1 GB and 12 GB success and cancellation runs. Peak RSS stays
below 20 MiB, measured temporary disk stays below the conservative estimate,
source hashes remain unchanged, and cancellation leaves no output or run files.

The later [12 GB sort optimization](benchmarks/2026-08-23-12gb-sort-performance.md)
reduced the 117,168,829-row `FIRSTNAME` sort from 335.837 seconds to 142.211
seconds while preserving byte-identical output and millisecond cancellation.

## Phase 7 — Hardening
Persistent indexes, invalidation, malformed-data UX, encoding strategy,
packaging/signing/notarization, accessibility audit, controlled cold-cache
performance runs, benchmark dashboard.

### Phase 7A checklist: desktop packaging and installation

- [x] Build the locked release binary and `target/package/Quarry.app` candidate
  through one repeatable command with stable name, identifier, executable,
  icon, version, build number, commit, source status, and architecture metadata.
- [x] Install or update only canonical `/Applications/Quarry.app`; when a prior
  app exists, preserve a verified rollback archive outside `/Applications` and
  restore it on failure. Remove legacy and staging bundles after verification.
- [x] Apply and strictly verify a consistent ad-hoc signature. Add Developer ID
  signing and notarization only when the required identity and credentials are
  available.
- [x] Smoke-test opening, inline editing, stable sorting, Save As, exact source
  preservation, exact output, quit, relaunch, and reopen from the installed app.
- [x] Document package, installation, update, verification, rollback, and
  signing limits in the [macOS packaging guide](MACOS_PACKAGING.md).
- [x] Commit Phase 7A, rebuild and install from that clean source, and verify
  `QuarrySourceStatus=clean` with `QuarryGitRevision` equal to the release commit.

**Phase 7A exit:** the package workflow is repeatable from the same checkout,
Rust toolchain, and macOS SDK. The installed app identifies its build, launches
the current binary from one canonical location, and passes the packaged-app
smoke test.

**Phase 7A exit met:** the clean implementation commit and installed bundle
match in the [packaged-app validation](benchmarks/2026-08-21-packaged-app.md).

### Phase 7B checklist: workflow polish and feature-gap review

- [ ] Dogfood opening, navigation, column selection, filters, transformations,
  editing, sorting, Save, Save As, discard, Undo, and Redo as connected desktop
  workflows.
- [ ] Refine discoverability, feedback, terminology, and interaction details in
  existing features before adding new workflows.
- [ ] Complete focused keyboard, accessibility, appearance, and window-size
  checks across the current interface.
- [ ] Inventory missing editor features after the polish pass and prioritize
  them by user value, implementation risk, and large-file constraints.

**Phase 7B exit:** current core workflows feel coherent and usable, and the
remaining feature gaps are documented in priority order.

## Later possibilities
Cross-platform front ends, plugin/API surface, schema inference, SQL-like querying, compressed files, CLI recipes, multi-file operations.

## Performance ladder
1. 1 GB — development baseline
2. 10 GB — first product promise
3. 25 GB — stress
4. 50 GB — serious stress
5. File larger than available RAM — architectural proof
