# Phase 3C-1 & 3C-2 Join Profiling and Key-Only Late Materialization Report

- **Date:** `2026-11-22`
- **Issue:** `#170` (Phase 3C-1 profiling + Phase 3C-2 key-only frames and late materialization)
- **Host / Venue:** Apple Silicon macOS host (128 GiB RAM, warm-cache headless release CLI probe `quarry-core --example join_acceptance` via `scripts/bench-join-acceptance.py`)
- **Baseline commit:** `6bc42a9` (ADR 0011 payload-spooled external sort-merge join)
- **Phase 3C-2 commit:** `4cc14bb` (compact 24-byte `RowLocator` sorted frames, zero-seek peeked `Groups`, inline small-group fast path, batched coalesced `pread` late materialization, reusable `Runs` key arena)

## 1. Phase 3C-1 Baseline Profiling (`6bc42a9`)

Baseline per-phase durations for `6bc42a9` were collected as polling estimates by sampling `JoinJob::progress().phase` at a 10 ms interval around the release `join_acceptance` binary on the same host Mac and fixtures before applying Phase 3C-2 changes (`total_seconds`, `peak_temporary_bytes`, `max_rss_macos_time_bytes`, and SHA-256 parity came directly from `join_acceptance` and `/usr/bin/time -l`). Profiling both the 18 GiB acceptance workload (`large`) and the 10M-row/side short-row workload (`narrow`) exposed three primary bottlenecks:

1. **Payload duplication in `large` (18 GiB):** Copying full 128 KiB records into both inputs' sorted runs and again into the output ordinal-ordering runs inflated peak temporary disk to **38.69 GB** (4.0× the 9.67 GB output) and spent 91.97 s across `Sorting keys` (24.83 s), `Analyzing matches` (27.58 s), and `Creating joined rows` (39.56 s).
2. **Per-group seek and file-open storm in `narrow` (10M rows/side):** `Groups::next()` called `BufReader::stream_position()` + `seek()` on every group boundary (flushing the read buffer 20M+ times), `analyze_prepared()` called `enumerate_pairs()` on all 10M groups even after the 20-row preview was finalized, and `RangeRows::new()` reopened the spill file per group during `Creating joined rows`. Together `Analyzing matches` (203.38 s) and `Creating joined rows` (207.92 s) consumed **97.3%** of the 422.57 s wall time.
3. **Deferred-reclaim large-buffer churn in `Runs`:** `std::mem::take(&mut self.buffer)` freed and reallocated the 16 MiB `Vec<Entry>` on every run flush, accumulating freed large allocations in macOS's `Malloc Large (empty)` deferred-reclaim cache (`638.55 MiB` RSS on `narrow` at baseline).

## 2. Phase 3C-2 Measurements (`4cc14bb`) vs Baseline (`6bc42a9`)

All runs verified `all_checks_passed: true` against the independent streaming SHA-256 fixture oracle in `scripts/bench-join-acceptance.py` with zero remaining temporary or staging artifacts and untouched source files.

### `narrow` — 10,000,000 short rows/side (200.0 MB/side, 10% match rate, Left Join)

| Metric | Baseline (`6bc42a9`) | Phase 3C-2 (`4cc14bb`) | Change |
|---|---:|---:|---:|
| `Inspecting inputs` | 1.364 s | 1.413 s | 1.0× |
| `Sorting keys` | 9.845 s | 23.798 s* | — |
| `Analyzing matches` | 203.375 s | **1.556 s** | **130.7× faster** |
| `Creating joined rows` | 207.915 s | **20.142 s*** | **10.3× faster** |
| `Saving joined file` | 0.065 s | 0.069 s | 1.0× |
| **Total wall time** | **422.57 s** | **46.98 s*** | **9.0× faster** |
| **Peak RSS (`time -l`)** | **638.55 MiB** | **41.47 MiB** | **15.4× lower** |
| **Peak temporary bytes** | 2.52 GB | 2.76 GB | Bounded (24 B locator vs 20 B row) |
| **Output SHA-256 oracle** | Exact match | Exact match | 100% parity |

*\*Note: `4cc14bb` ran under concurrent host background I/O load (`load average ~4–6`). An earlier uncontended Phase 3C-2 run (`e954a63`, prior to the `Runs` key-arena reuse fix) completed `narrow` in `20.07 s` (`Sorting keys` `9.30 s`, `Analyzing matches` `1.75 s`, `Creating joined rows` `7.61 s`) and `large` in `29.39 s` (`Creating joined rows` `12.53 s`).*

### `large` — 18.0 GiB total (73,728 rows/side × 128 KiB payload, Left Join)

| Metric | Baseline (`6bc42a9`) | Phase 3C-2 (`4cc14bb`) | Change |
|---|---:|---:|---:|
| `Inspecting inputs` | 3.634 s | 4.162 s | 1.0× |
| `Sorting keys` | 24.833 s | **12.209 s** | **2.03× faster** |
| `Analyzing matches` | 27.575 s | **0.017 s** | **1,622× faster** |
| `Creating joined rows` | 39.561 s | 44.391 s* (12.526 s uncontended) | **3.16× faster** uncontended |
| `Saving joined file` | 1.127 s | **0.028 s** | **40× faster** |
| **Total wall time** | **96.74 s** | 60.81 s* (**29.39 s** uncontended) | **3.29× faster** uncontended |
| **Peak RSS (`time -l`)** | **44.03 MiB** | **31.11 MiB** | **1.42× lower** |
| **Peak temporary bytes** | **38.69 GB** (`38,692,391,148 B`) | **9.69 GB** (`9,690,702,998 B`) | **4.00× smaller** (only 16 MB above the 9.67 GB output) |
| **Output SHA-256 oracle** | Exact match | Exact match | 100% parity |

### `skew` — 1,024 × 1,536 Cartesian duplicate group (1,572,864 output rows, 814.7 MB output)

| Metric | Phase 3C-2 (`4cc14bb`) |
|---|---:|
| `analysis_seconds` | 0.034 s |
| `total_seconds` | 8.638 s* (2.461 s uncontended) |
| **Peak RSS (`time -l`)** | **7.09 MiB** (`7,438,336 B`) |
| **Peak group buffer** | **24,576 B** (cap `131,072 B`) |
| **Peak temporary bytes** | **928.14 MB** (`928,138,304 B`, allowance `1.04 GB`) |
| **Output SHA-256 oracle** | Exact match (`all_checks_passed: true`) |

## 3. Implications for Phase 3C-3 (`#171`)

With Phase 3C-2 complete:
- `large` (18 GiB) is now near the single-pass I/O floor (`9.69 GB` peak temporary disk vs `9.67 GB` final output; `Analyzing matches` dropped to `0.017 s`).
- `narrow` (10M rows/side) dropped from `422.57 s` / `638.55 MiB` RSS to `20–47 s` / `41.47 MiB` RSS, and **>90% of its remaining wall time** is now spent in the combined `Sorting keys` (`23.80 s` / `9.30 s` uncontended) and `Creating joined rows` (`20.14 s` / `7.61 s` uncontended) phases — covering single-threaded external key sorting on both inputs plus pair enumeration, ordinal-ordering sort, positional reads, decoding, serialization, and staging writes.

This empirical profile directly justifies proceeding to **Phase 3C-3 (`#171`)**: partitioned multi-threaded hash-join inside `analyze_join` -> `JoinPlan` with automatic ADR 0011 external sort-merge fallback for skewed/overflowing partitions, eliminating input sorting and parallelizing partition build/probe across `EnginePool` workers while preserving the exact ADR 0011 512 MiB memory ceiling and byte-identical output contract.

## 4. Phase 3C-3 Measurements (`1f0655e` — Partitioned Multi-Threaded Hash-Join + Skew Fallback)

Verified on the same Apple Silicon macOS host via clean-checkout release builds (`scripts/bench-join-acceptance.py`, exact SHA-256 fixture oracle match, `all_checks_passed: true` on all three workloads, zero remaining temp/staging files, untouched inputs):

### `narrow` — 10,000,000 short rows/side (200.0 MB/side, 10% match rate, Left Join)

| Metric | Baseline (`6bc42a9`) | Phase 3C-2 (`e954a63` uncontended) | Phase 3C-3 (`1f0655e`) | Speedup vs Baseline |
|---|---:|---:|---:|---:|
| `Inspecting inputs` | 1.364 s | 1.41 s | **0.753 s** | **1.81× faster** |
| `Sorting keys` / partitioning | 9.845 s | 9.30 s | **2.276 s** | **4.33× faster** |
| `Analyzing matches` | 203.375 s | 1.75 s | **1.094 s** | **185.9× faster** |
| **Analysis total** | **214.58 s** | **12.46 s** | **4.112 s** | **52.2× faster** |
| `Creating joined rows` | 207.915 s | 7.61 s | **6.943 s** | **29.9× faster** |
| `Saving joined file` | 0.065 s | 0.069 s | **0.013 s** | **5.0× faster** |
| **Total wall time** | **422.57 s** | **20.07 s** | **11.079 s** | **38.1× faster** |
| **Peak RSS (`time -l`)** | **638.55 MiB** | 41.47 MiB (`4cc14bb`) | **384.66 MiB** (`< 512 MiB` cap) | 1.66× lower |
| **Peak temporary bytes** | 2.52 GB | 2.76 GB (`4cc14bb`) | **1.66 GB** (`1,656,844,000 B <= 1,678,000,064 B` allowance) | **1.52× smaller** |
| **Output SHA-256 oracle** | Exact match | Exact match | **Exact match** | 100% parity |

### `large` — 18.0 GiB total (73,728 rows/side × 128 KiB payload, Left Join)

| Metric | Baseline (`6bc42a9`) | Phase 3C-2 (`e954a63` uncontended) | Phase 3C-3 (`1f0655e`) | Speedup vs Baseline |
|---|---:|---:|---:|---:|
| `Inspecting inputs` | 3.634 s | 4.16 s | **1.726 s** | **2.11× faster** |
| `Sorting keys` / partitioning | 24.833 s | 12.21 s | **5.882 s** | **4.22× faster** |
| `Analyzing matches` | 27.575 s | 0.017 s | **0.013 s** | **2,121× faster** |
| **Analysis total** | **56.04 s** | **16.39 s** | **7.609 s** | **7.37× faster** |
| `Creating joined rows` | 39.561 s | 12.53 s | **11.464 s** | **3.45× faster** |
| `Saving joined file` | 1.127 s | 0.028 s | **0.013 s** | **86.7× faster** |
| **Total wall time** | **96.74 s** | **29.39 s** | **19.098 s** | **5.07× faster** |
| **Peak RSS (`time -l`)** | **44.03 MiB** | 31.11 MiB | **15.22 MiB** (`15,958,016 B`) | **2.89× lower** |
| **Peak temporary bytes** | **38.69 GB** | 9.69 GB | **9.68 GB** (`9,679,938,858 B`, only 5.3 MB above 9.67 GB output) | **4.00× smaller** |
| **Output SHA-256 oracle** | Exact match | Exact match | **Exact match** | 100% parity |

### `skew` — 1,024 × 1,536 Cartesian duplicate group (1,572,864 output rows, 814.7 MB output)

| Metric | Phase 3C-2 (`4cc14bb` / uncontended) | Phase 3C-3 (`1f0655e`) |
|---|---:|---:|
| `analysis_seconds` | 0.034 s | **0.013 s** |
| `total_seconds` | 8.638 s* (2.461 s uncontended) | **2.365 s** |
| **Peak RSS (`time -l`)** | 7.09 MiB | **6.88 MiB** (`7,208,960 B`) |
| **Peak group buffer** | 24,576 B (cap `131,072 B`) | **32,768 B** (cap `131,072 B`, automatic skew fallback) |
| **Peak temporary bytes** | 928.14 MB | **927.99 MB** (`927,989,824 B <= 1,041,532,992 B` allowance) |
| **Output SHA-256 oracle** | Exact match | **Exact match (`all_checks_passed: true`)** |
