# Quarry Roadmap

The roadmap is ordered by technical risk rather than feature excitement.

## Current progress — 2026-08-15

| Phase | Status | Evidence |
|---|---|---|
| Phase 0 — Foundation | Complete | Rust workspace, CI, deterministic generator, ADR, licensing, and the [12 GB benchmark](benchmarks/2026-08-14-large-file.md) |
| Phase 1 — Prove the core | Complete | Progressive open, correct parsing, bounded indexing, live navigation, cancellation, and the measured [no-cache decision](adr/0002-defer-viewport-cache.md) |
| Phase 2 — UI bake-off | Complete | The [egui](benchmarks/2026-08-14-egui-spike.md) and [AppKit](benchmarks/2026-08-14-appkit-spike.md) candidates were measured; [ADR 0003](adr/0003-select-egui-ui.md) selects egui |
| Phase 3 — Viewer alpha | In progress | Continuous, bounded-memory logical scrolling is implemented and measured on the [12 GB reference file](benchmarks/2026-08-15-continuous-scroll.md) |

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
- [ ] Complete the remaining viewer-alpha grid and file controls.

**Phase 3 in progress:** this vertical slice provides file-level continuous
scrolling while indexing expands the available range without changing the
current row. The [12 GB run](benchmarks/2026-08-15-continuous-scroll.md) records
no animation hitches and stays below the memory ceiling. Search, filtering,
editing, and cosmetic redesign remain outside this slice.

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
Virtualized grid, drag/drop/open, delimiter/header controls, column operations, jump-to-row, streaming search, copy, status, diagnostics.

**Exit:** Quarry is genuinely useful for inspecting huge files.

## Phase 4 — Filters and export
Contains/equality filters, multiple predicates, incremental results, filtered navigation, streaming export, progress/cancellation.

## Phase 5 — Column transformations
Split, join, rename, reorder/drop, find/replace, preview, reusable transformation pipeline.

## Phase 6 — Sorting
Type semantics, external run generation, spill management, merge, stable row-order abstraction, disk-space estimation.

## Phase 7 — Hardening
Persistent indexes, invalidation, malformed-data UX, encoding strategy, packaging/signing/notarization, accessibility audit, benchmark dashboard.

## Later possibilities
Cross-platform front ends, plugin/API surface, schema inference, SQL-like querying, compressed files, CLI recipes, multi-file operations.

## Performance ladder
1. 1 GB — development baseline
2. 10 GB — first product promise
3. 25 GB — stress
4. 50 GB — serious stress
5. File larger than available RAM — architectural proof
