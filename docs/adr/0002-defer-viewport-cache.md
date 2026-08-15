# ADR 0002: Defer a viewport cache

**Status:** Accepted
**Date:** 2026-08-14

## Context

Phase 1 needs an evidence-based cache decision before the UI bake-off. The
engine should keep post-index viewport reads below 8 ms at p95, leaving roughly
half of a 60 Hz frame for rendering and interaction.

## Options considered

1. Keep the current checkpoint-based reads and rely on the operating system's
   file cache.
2. Add a bounded application cache for file chunks or decoded viewports.

## Decision

Do not add an application cache yet. A release build ran 500 repeated, 500
sequential, and 500 seeded-random 100-row reads against the warm 11.33 GiB
reference file after structural indexing:

| Pattern | p50 | p95 | Max |
|---|---:|---:|---:|
| Repeated | 1.686 ms | 1.824 ms | 1.976 ms |
| Sequential | 1.690 ms | 1.808 ms | 2.073 ms |
| Random | 1.699 ms | 1.843 ms | 1.959 ms |

The slowest p95 is more than four times inside the 8 ms budget. The benchmark
is reproducible with `quarry-bench viewport` and uses the same public session,
index, and row-read path that a UI will call.

## Consequences

Quarry avoids cache budgets, eviction, invalidation, and duplicate memory while
they provide no measured benefit. These results describe post-index warm access
on a 128 GiB Apple M3 Max, not cold random I/O on lower-memory machines.

Reconsider a bounded cache if UI frame benchmarks miss their budget, viewport
p95 exceeds 8 ms on supported hardware, or cold/random workloads on
memory-constrained machines show a material regression.
