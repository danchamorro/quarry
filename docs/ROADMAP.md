# Quarry Roadmap

The roadmap is ordered by technical risk rather than feature excitement.

## Phase 0 — Foundation
Rust workspace, CI, lint/test policy, deterministic large-file generator, benchmark harness, 1 GB/10 GB profiles, CLI experiments, ADR process, and license decision.

**Exit:** reproducible benchmark results on a known machine.

## Phase 1 — Prove the core
Open/map huge files, sample format, parse first viewport, background structural indexing, bounded cache, row-range navigation, cancellation, and diagnostics.

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
