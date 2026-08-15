# Quarry Roadmap

The roadmap is ordered by technical risk rather than feature excitement.

## Current progress — 2026-08-14

| Phase | Status | Evidence |
|---|---|---|
| Phase 0 — Foundation | Complete | Rust workspace, CI, deterministic generator, ADR, licensing, and the [12 GB benchmark](benchmarks/2026-08-14-large-file.md) |
| Phase 1 — Prove the core | In progress | Progressive open, correct parsing, bounded structural index, live row navigation, cancellation, and diagnostics are working |
| Phase 2 — UI bake-off | Not started | Begins only after the remaining Phase 1 cache decision |

### Phase 1 checklist

- [x] Return the first viewport before full-file indexing.
- [x] Parse quoted delimiters, escaped quotes, CRLF, and embedded newlines.
- [x] Keep structural-index memory bounded with adaptive checkpoints.
- [x] Navigate a row range from a completed index.
- [x] Navigate a row range as soon as the live index reaches it.
- [x] Cancel and join background indexing promptly.
- [x] Report first-row time, throughput, progress, memory, and index size.
- [ ] Measure repeated nearby and random viewport reads, then add a bounded
  cache only if the measurements justify it.

**Current exit gate:** make and document the cache/no-cache decision. The core
already opens and navigates the 12 GB reference file with bounded memory.

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
