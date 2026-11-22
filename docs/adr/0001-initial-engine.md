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

## Amendment (2026-11-21 — Phase 2 High-Throughput Parallel Engine & I/O Strategy, T5)

### Context

Scaling Quarry from single-threaded byte-at-a-time scanning to multi-gigabyte
workloads exposed two architectural constraints:

1. Memory-mapping (`mmap`) user source files raises unrecoverable `SIGBUS`
   faults on Unix if an external writer truncates the file or a network volume
   drops out; source-stamp checks cannot intercept an asynchronous page fault.
2. Row-strided checkpoints can emit unbounded per-chunk checkpoint counts on
   dense newline-heavy inputs (`\n\n\n`), inflating worker result buffers.

### Decision

- **Drop `mmap` for user source files:** Perform concurrent reads through
  positional OS calls (`FileExt::read_at` / `pread` on Unix with `F_RDADVISE`
  on macOS and `posix_fadvise(POSIX_FADV_SEQUENTIAL)` on Linux;
  `FileExt::seek_read` on Windows). External truncation surfaces cleanly as
  `QuarryError::SourceChanged` or `QuarryError::Io`. (`mmap` remains eligible
  only for Quarry-owned immutable internal working/spill files if separately
  measured and approved.)
- **64-byte SIMD structural classification:** Use runtime-dispatched 64-byte
  SIMD classifiers (NEON + `pmull` on AArch64, AVX2 + `pclmulqdq` on x86_64)
  with violation-bit scalar replay for exact error parity on ASCII-safe
  single-byte encodings (`UTF-8`, `Latin-1`, `Windows-1252`). Keep UTF-16 LE/BE
  on the scalar encoding-aware path.
- **LF-aligned chunk-parallel indexing with in-worker dual-state execution:**
  Partition eligible large files into 1–2 MiB windows aligned immediately after
  a literal `\n`, where only two entry states (`FieldStart` or `Quoted`) are
  possible under quote-scoped escapes. Workers evaluate both states in-worker
  when quotes are present (O(1) trivial `Quoted` state on quote-free chunks) so
  ordered merging resolves state in O(1) without serial rescan stalls. Giant
  single-line spans lacking `\n` within the bounded alignment window fall back
  to bounded sequential state carry.
- **Byte-stride checkpoints under the 16 MiB budget:** Parallel indexing emits
  one checkpoint at the first record end at or after every byte stride `S`
  (256 KiB initial default, compacting by doubling `S` when reaching the budget)
  so worker result buffers and worst-case seek latency remain bounded in bytes.
- **Engine-owned bounded worker pool:** Use a pure-`std` worker pool
  (`std::thread`, foreground-first priority queue, shared byte budget, macOS
  `QOS_CLASS_USER_INITIATED` thread hint, default `available_parallelism() - 1`,
  configurable via CLI `--threads`) without adding Rayon or external runtime
  dependencies.

### Consequences

- Multi-core indexing scales across CPU cores while maintaining 100% parity
  with the pure-scalar reference oracle and flat bounded RSS (`<= 64 MiB`
  indexing envelope).
- Public claims referencing `mmap` (`C02` in `docs/V1_CLAIM_LEDGER.md`) are
  rephrased to parallel positional `pread` / `seek_read` I/O.
