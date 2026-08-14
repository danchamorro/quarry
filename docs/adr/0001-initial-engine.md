# ADR 0001: Minimal progressive engine

**Status:** Accepted
**Date:** 2026-08-14

## Context

Quarry must return a useful viewport from files larger than RAM, then build a
correct structural index without memory growing per record. CSV record
boundaries depend on quote state, so viewport and indexing code cannot use
different newline rules.

## Decision

- Keep the first workspace to `quarry-delimited`, `quarry-core`, and
  `quarry-cli`.
- Use one byte-oriented state machine for viewport and index record boundaries.
- Use fixed-size buffered reads and seeks. Do not memory-map until a measured
  random-access bottleneck justifies it.
- Store a checkpoint every 4,096 records under a 16 MiB budget. When the budget
  is reached, retain every second checkpoint and double the interval.
- Materialize only the requested viewport; the parser borrows unescaped fields.
- Cap bootstrap and individual-record materialization at 64 MiB and fail
  explicitly when exceeded.
- Expose indexing as a cancellable background job with progress snapshots.

`memchr` is the only hot-path dependency because it replaces byte-at-a-time
search for delimiters, quotes, and newlines. `libc` is confined to the CLI for
macOS peak-RSS reporting. `memmap2`, Rayon, `thiserror`, Criterion, persistent
indexes, caches, and extra service crates are deferred until measurements show
they are needed.

## Consequences

Initial row-range reads scan forward from the nearest checkpoint. The worst-case
scan grows if a truly enormous file forces checkpoint compaction, but index RAM
remains bounded. The first version assumes byte-compatible delimiters and leaves
broader encoding policy open. Indexes are rebuilt per session until persistence
and invalidation rules are designed.
