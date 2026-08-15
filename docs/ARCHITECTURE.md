# Quarry Architecture

## Objective
Quarry must operate on files larger than available RAM. Treat it as a **large-file data engine with a desktop interface**, not a conventional text editor that happens to parse CSV.

## High-level design
```text
macOS UI
   |
Application API
   +-- Session / Job Coordinator
   +-- Viewport Service
   +-- Search / Filter Service
   +-- Transform Pipeline
   |
Rust Engine
   +-- File Access
   +-- Format Detection
   +-- Delimited Parser
   +-- Structural Index
   +-- Bounded Cache
   +-- Search / Query
   +-- External Sort
   +-- Export Writer
   +-- Diagnostics / Benchmarks
```

The UI framework is replaceable. The Rust engine is the performance foundation.

## Proposed workspace
```text
quarry/
  crates/
    quarry-core/
    quarry-io/
    quarry-delimited/
    quarry-index/
    quarry-query/
    quarry-transform/
    quarry-bench/
    quarry-ffi/
  apps/
    quarry-cli/
    quarry-macos/
  benches/
  fixtures/
  docs/
```

The first implementation intentionally uses only `quarry-delimited`,
`quarry-core`, and `quarry-cli`. File access and structural indexing remain
modules in core until their APIs or dependencies justify separate crates. See
[ADR 0001](adr/0001-initial-engine.md).

## File access
Evaluate memory mapping for viewport-oriented random access and buffered sequential I/O for indexing, search, export, and sorting. `memmap2` is a likely Rust candidate, but access patterns must be benchmarked.

## Progressive open
Bootstrap by sampling a bounded region and parsing enough rows for the first viewport. Return that viewport immediately. Continue structural indexing in background workers.

## Structural indexing
Delimited files cannot be indexed correctly by blindly finding newline bytes because quoted fields may contain line breaks. The indexer must track dialect and quote state.

Possible structures: coarse byte checkpoints, exact record offsets at intervals, chunk metadata, and optional persistent sidecar indexes with robust invalidation.

## Parsing
Optimize common paths without compromising quoted fields, escaped quotes, embedded delimiters/newlines, CRLF/LF, UTF-8 boundaries, malformed records, wide rows, or giant fields. Avoid allocating an owned string for every cell; materialize only what the viewport needs.

## Viewport
The UI requests a bounded window such as rows 8,250,000–8,250,150 and columns 4–18. The engine resolves and parses only the required region plus modest overscan.

## Bounded caching
File chunks, row boundaries, decoded visible cells, search results, and filter metadata all receive explicit memory budgets. No cache grows indefinitely with file size.

## Concurrency
Rust workers handle indexing, search, filters, sorting, export, and prefetching. Evaluate `rayon` for suitable CPU-parallel work, but use explicit coordination where cancellation/lifecycle control requires it. Every long operation must be cancellable.

## Search and filters
Search begins as a high-throughput sequential scan that streams matches incrementally. Filters can use streaming predicates and compact row/chunk selection metadata instead of copying matching rows.

## Sorting
Use a disk-aware external merge sort: bounded runs, sort in memory, spill to temporary storage, then merge into a stable row-order abstraction. Sorting must not delay the first performance milestone.

## Transformations
Model split/join/reorder/drop operations as a non-destructive pipeline ending in streaming export.

## UI selection
Do not select a framework because it is fashionable, native, or Rust-based. Prototype serious candidates against the same engine. Candidates may include AppKit, SwiftUI with lower-level AppKit components, egui, Slint, and—only if profiling justifies it—a custom GPU-backed grid.

Measure scrolling/frame time, allocation behavior, text rendering, keyboard behavior, VoiceOver, native integration, FFI complexity, packaging, and developer velocity.

[ADR 0003](adr/0003-select-egui-ui.md) records the measured egui/AppKit
bake-off and selects egui while keeping this boundary replaceable.

## Benchmarking
Generate deterministic 1 GB, 10 GB, 25 GB, and 50 GB datasets locally, including multiline quoted fields, wide tables, long fields, and malformed records.

Track time-to-first-rows, memory, index/search/export throughput, scroll frame time, cache behavior, and cancellation latency.

## Architecture rule
**If a feature only works because the entire file fits in RAM, it is not a finished Quarry feature.**
