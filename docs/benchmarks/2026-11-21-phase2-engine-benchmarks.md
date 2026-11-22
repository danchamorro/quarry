# Phase 2 Engine Core Benchmarks & Golden Digest Verification: 2026-11-21

This report records the Phase 2.7 (`#161`) split speed-attribution benchmarks and full 30-workload
golden output digest verification for Quarry's Phase 2 high-throughput engine core (parameterized
64-byte SIMD block classifier, LF-aligned dual-state parallel indexing, parallel positional
`pread`/`seek_read` I/O, bounded engine worker pool, parallel literal Find / Filter / Filtered
Export, and hardened persistent sidecar structural index cache).

All measurements ran on a single clean merged `main` revision (`fd78d37d95e2fe28e8c74a8bbea97b38dede500c`)
against the frozen Phase 1 Gate 1A baseline
([2026-11-21-phase1-public-datasets-baseline.md](2026-11-21-phase1-public-datasets-baseline.md)) and
machine-readable golden digests
([2026-11-21-phase1-public-datasets-golden.json](2026-11-21-phase1-public-datasets-golden.json)).

## Environment & Provenance

| Item | Value |
|---|---|
| Venue | `host-bare-metal` (headless CLI only; desktop `.app` remains VM-only) |
| Machine | MacBook Pro, Apple M3 Max, 16 cores, 128 GiB RAM |
| Operating system | macOS 27.0.1 (`26A434`), `arm64` |
| Rust | `rustc 1.88.0 (6b00bc388 2025-06-23)` (`aarch64-apple-darwin`) |
| Engine revision | `fd78d37d95e2fe28e8c74a8bbea97b38dede500c` (`clean`) |
| Frozen `quarry-bench` SHA-256 | `bf81d0af55b794f470d93885759572a099630b0f9456141fbba5a83412165125` |
| Active SIMD backend | `neon` (AArch64 NEON + `vmull_p64` carryless prefix-XOR) |
| Worker configuration | `--threads 8` (and explicit `1, 2, 4, 8` scaling sweep) |
| Cache protocol | `warm` — Full SHA-256 read before suite start; isolated `0700` `QUARRY_INDEX_CACHE_DIR` |
| Suite Started / Finished (UTC) | `2026-11-22T03:45:28.025033+00:00` -> `2026-11-22T04:35:58.606624+00:00` |
| Sources unchanged | `True` (exact SHA-256 match before and after) |
| Golden output parity | **14/14 output-producing workloads (30/30 total)** matched Phase 1 SHA-256, bytes, and quote-aware row counts (`0` unterminated quotes) |

## Verified Inputs

| Dataset | File | Bytes | Rows | SHA-256 |
|---|---|---:|---:|---|
| `weather` | `daily_weather_preprocessed_1896_2023.csv` | 9,770,967,575 (9.10 GiB) | 131,985,330 | `b775c8b7e6d39643bc1e10874deecc9b067e2866c79a8e647a6c93550412d47f` |
| `hacker-news` | `hacknernews.csv` | 12,718,039,463 (11.84 GiB) | 28,737,558 | `165d9336b6266519da4e99d0348576537f80337e40d1794aefb00258a49a0373` |

---

## 1. Single-Thread SIMD (`neon`) vs Pure-Scalar Oracle

Measured via `cargo test --release -p quarry-delimited --test scalar_oracle --locked -- --ignored --nocapture`
(provisional Phase 2 gate: `>= 3×` single-thread speedup; oracle parity verified across all 8 non-ignored
oracle tests and all 11 generated torture-corpus fixtures):

| Workload | Scalar | SIMD (`neon`) | Single-Thread Speedup | Gate (`>= 3×`) |
|---|---:|---:|---:|---|
| Unquoted weather-like | 0.23 GB/s (293.171 ms) | 9.52 GB/s (7.048 ms) | **41.60×** | Pass |
| Quoted Hacker News-like | 0.87 GB/s (77.461 ms) | 7.23 GB/s (9.279 ms) | **8.35×** | Pass |

---

## 2. Parallel Scaling (`1, 2, 4, 8` Threads) & Dual-Scan Telemetry

Measured on the frozen `quarry-bench` binary with explicit `--no-index-cache` on `open` (pure cache-miss
structural indexing) and warm sidecar on `search` (pure scan-only Find time separated from index load).
All filtered exports verified byte-identical to Phase 1 golden SHA-256 digests at every thread count.

### Weather (`daily_weather_preprocessed_1896_2023.csv`, 9.10 GiB, 131,985,330 rows)

| Threads | `open` Miss (s) | Index Throughput | Index Peak RSS | `find-absent` Scan (s) | `filter-and` Scan (s) | `export` (s) | Chunk Telemetry |
|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 2.215 | 4.11 GiB/s | 5.64 MiB | 2.062 | 7.890 | 38.280\* | 9,319 chunks (0 dual-scan, 0 quoted-entry, 0.00 B rescanned) |
| 2 | 1.120 | 8.13 GiB/s | 9.31 MiB | 1.036 | 4.098 | 4.292 | 9,318 chunks (0 dual-scan, 0 quoted-entry, 0.00 B rescanned) |
| 4 | 0.575 | 15.84 GiB/s | 14.97 MiB | 0.560 | 2.097 | 2.235 | 9,318 chunks (0 dual-scan, 0 quoted-entry, 0.00 B rescanned) |
| **8** | **0.311** | **29.22 GiB/s** | **20.16 MiB** | **0.303** | **1.086** | **1.161** | **9,318 chunks (0 dual-scan, 0 quoted-entry, 0.00 B rescanned)** |

- Scaling efficiency (1 -> 8 threads): `open` **7.12×** (89% efficiency), `find-absent` **6.81×** (85%),
  `filter-and` **7.27×** (91%).
- \*At `--threads 1`, `export` uses the legacy single-threaded path (`export.rs`), whereas `--threads >= 2`
  uses `export_parallel.rs` with the zero-allocation unquoted UTF-8 predicate fast path (`4.292 s` at 2 threads ->
  `1.161 s` at 8 threads, **3.70×** from 2 -> 8 threads).

### Hacker News (`hacknernews.csv`, 11.84 GiB, 28,737,558 rows)

| Threads | `open` Miss (s) | Index Throughput | Index Peak RSS | `find-absent` Scan (s) | `filter-and` Scan (s) | `export` (s) | Chunk Telemetry |
|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 3.136 | 3.78 GiB/s | 5.81 MiB | 3.411 | 10.220 | 11.202 | 12,129 chunks (0 dual-scan, 0 quoted-entry, 0.00 B rescanned) |
| 2 | 1.723 | 6.87 GiB/s | 8.86 MiB | 1.864 | 6.786 | 5.695 | 12,125 chunks (12,124 dual-scan, 566 quoted-entry, 0.00 B rescanned) |
| 4 | 0.914 | 12.96 GiB/s | 20.91 MiB | 0.975 | 2.934 | 2.988 | 12,125 chunks (12,124 dual-scan, 566 quoted-entry, 0.00 B rescanned) |
| **8** | **0.472** | **25.12 GiB/s** | **24.95 MiB** | **0.510** | **1.617** | **1.663** | **12,125 chunks (12,124 dual-scan, 566 quoted-entry, 0.00 B rescanned)** |

- Scaling efficiency (1 -> 8 threads): `open` **6.64×** (83% efficiency), `find-absent` **6.69×** (84%),
  `filter-and` **6.32×** (79%), `export` **6.74×** (84%).
- In-worker dual-state execution processed all 566 quoted-entry boundaries in `hacknernews.csv` with
  **0.00 B rescanned** at merge time.

---

## 3. Split Latency & Gate Summary (`--threads 8`, Warm Cache)

| Metric | Target / Gate | Measured Result | Status |
|---|---|---|---|
| Weather first-100-rows bootstrap | `<= 10 ms` | **0.787 ms** (8-thread miss); **0.815–0.859 ms** (5 hit runs) | Pass |
| Hacker News first-100-rows bootstrap | `<= 10 ms` | **0.772 ms** (8-thread miss); **0.774–0.886 ms** (5 hit runs) | Pass |
| `C04` 10M-row synthetic CSV warm CLI open (`346.7 MB`, 10,000,001 rows, 5 runs) | `< 50 ms` | **0.826 ms** median first-100-rows (`0.750–0.963 ms`); **10.440 ms** median whole-process wall (`10.150–13.911 ms`), `4.58 MiB` RSS | Pass (CLI; GUI paint Phase 4B) |
| Weather full index cache miss (warm) | `<= 6 s` | **0.311 s** (`29.22 GiB/s`, `20.16 MiB` peak RSS) | Pass |
| Hacker News full index cache miss (warm) | `<= 3 s` | **0.472 s** (`25.12 GiB/s`, `24.95 MiB` peak RSS) | Pass |
| Weather validated sidecar hit + row `100,000,000` seek (5 runs) | `< 50 ms` | **4.395 ms** median cache load + **0.235 ms** jump; **19.104 ms** median whole-process wall (`18.085–19.846 ms`), `5.08–5.94 MiB` RSS | Pass |
| Hacker News validated sidecar hit + row `20,000,000` seek (5 runs) | `< 50 ms` | **5.545 ms** median cache load + **0.332 ms** jump; **20.836 ms** median whole-process wall (`20.495–22.583 ms`), `5.08–6.16 MiB` RSS | Pass |
| Weather random viewport seek (500 × 100-row random reads) | Low-overhead `pread` | **0.231 ms p95** (`0.214 ms` p50, `4,634.9 req/s`, `6.44 MiB` peak RSS) | Pass |
| Hacker News random viewport seek (500 × 100-row random reads) | Low-overhead `pread` | **0.325 ms p95** (`0.294 ms` p50, `3,352.9 req/s`, `7.77 MiB` peak RSS) | Pass |
| Weather scan-only `find-absent` vs whole command | `<= 8 s` | **0.303 s** scan (`0.317 s` wall; `0.338 s` in full suite) | Pass |
| Weather scan-only `filter-and` vs whole command | `<= 8 s` | **1.086 s** scan (`1.097 s` wall; `1.069 s` in full suite) | Pass |
| Hacker News scan-only `find-absent` vs whole command | `<= 4 s` | **0.510 s** scan (`0.525 s` wall; `0.511 s` in full suite) | Pass |
| Hacker News scan-only `filter-and` vs whole command | `<= 4 s` | **1.617 s** scan (`1.659 s` wall; `1.573 s` in full suite) | Pass |
| Peak index RSS (8 threads) | `<= 64 MiB` | **20.16 MiB** (weather) / **24.95 MiB** (HN) | Pass |
| Cancellation shutdown latency (Weather, 8 threads, 10 samples each, 0 orphan files) | `< 25 ms` p95 | `search`: **1.280 ms p95** (median 1.267 ms); `filter`: **1.391 ms p95** (median 1.356 ms); `export`: **1.285 ms p95** (median 1.270 ms) | Pass |

---

## 4. Full 30-Workload Matched-Venue Comparison (Phase 1 vs Phase 2)

Executed via:
`python3 scripts/benchmark-public-datasets.py . /tmp/quarry-phase2-public-bench --expected-outputs docs/benchmarks/2026-11-21-phase1-public-datasets-golden.json --threads 8`

Every output-producing workload matched its frozen Phase 1 SHA-256 digest, byte count, and logical row
count with zero unterminated-quote outputs (`exit_code = 0` for 30/30 workloads; `sources_unchanged = true`).

| Dataset | Case | Phase 1 (s) | Phase 2 (s) | Speedup | Phase 2 Peak RSS | Peak Scratch | Golden Match |
|---|---|---:|---:|---:|---:|---:|---|
| `weather` | `open` | 37.936 | **0.458** | **82.8×** | 21.3 MiB | 0 | — |
| `weather` | `viewport` | 45.543 | **0.303** | **150.3×** | 6.4 MiB | 0 | — |
| `weather` | `find-early` | 38.791 | **0.013** | **2,983.9×** | 20.3 MiB | 0 | — |
| `weather` | `find-absent` | 110.366 | **0.338** | **326.5×** | 40.9 MiB | 0 | — |
| `weather` | `filter-numeric` | 70.412 | **1.058** | **66.6×** | 46.1 MiB | 0 | — |
| `weather` | `filter-and` | 68.644 | **1.069** | **64.2×** | 23.8 MiB | 0 | — |
| `weather` | `export` | 69.330 | **1.155** | **60.0×** | 24.5 MiB | 2.1 MiB | Exact (`4cc76824…`, 26,506 rows) |
| `weather` | `edit-save-as` | 84.779 | **9.315** | **9.1×** | 21.4 MiB | 9.10 GiB | Exact (`1dda7240…`, 131,985,330 rows) |
| `weather` | `replace-all` | 187.803 | 153.244 | 1.2× | 6.2 MiB | 9.10 GiB | Exact (`99df9f38…`, 131,985,330 rows) |
| `weather` | `split-auto` | 335.654 | 355.010 | 0.9× (Phase 3) | 29.6 MiB | 9.10 GiB | Exact (`a3473c04…`, 131,985,330 rows) |
| `weather` | `combine` | 271.298 | 441.560 | 0.6× (Phase 3) | 30.0 MiB | 9.10 GiB | Exact (`c5f57f5d…`, 131,985,330 rows) |
| `weather` | `sort-number` | 805.252 | 223.719 | 3.6× (Phase 3) | 100.6 MiB | 22.00 GiB | Exact (`4be7c669…`, 131,985,330 rows) |
| `weather` | `duplicates` | 271.080 | 673.355 | 0.4× (Phase 3) | 147.4 MiB | 33.04 GiB | — |
| `weather` | `duplicates-remove` | 264.782 | 262.692 | 1.0× (Phase 3) | 147.4 MiB | 33.03 GiB | Exact (`10e09e36…`, 127,261,910 rows) |
| `weather` | `sort-cancel` | 45.611 | **15.185** | **3.0×** | 53.2 MiB | 79.0 MiB | — |
| `hacker-news` | `open` | 7.812 | **4.244**\*\* | **1.8×** (warm: **0.472 s / 16.6×**) | 17.5 MiB | 0 | — |
| `hacker-news` | `viewport` | 8.815 | **0.421** | **20.9×** | 7.8 MiB | 0 | — |
| `hacker-news` | `find-early` | 7.226 | **0.015** | **481.7×** | 22.3 MiB | 0 | — |
| `hacker-news` | `find-absent` | 22.118 | **0.511** | **43.3×** | 39.1 MiB | 0 | — |
| `hacker-news` | `filter-numeric` | 14.360 | **1.640** | **8.8×** | 33.1 MiB | 0 | — |
| `hacker-news` | `filter-and` | 15.221 | **1.573** | **9.7×** | 33.8 MiB | 0 | — |
| `hacker-news` | `export` | 14.544 | **1.657** | **8.8×** | 28.8 MiB | 51.6 MiB | Exact (`1ebb1468…`, 128,223 rows) |
| `hacker-news` | `edit-save-as` | 21.492 | **10.497** | **2.0×** | 21.6 MiB | 11.84 GiB | Exact (`e8495618…`, 28,737,558 rows) |
| `hacker-news` | `replace-all` | 44.833 | 38.583 | 1.2× (Phase 3) | 8.2 MiB | 11.84 GiB | Exact (`d4e78f74…`, 28,737,558 rows) |
| `hacker-news` | `split-auto` | 79.997 | 56.159 | 1.4× (Phase 3) | 29.5 MiB | 11.46 GiB | Exact (`47e1d395…`, 28,737,558 rows) |
| `hacker-news` | `combine` | 67.062 | 45.476 | 1.5× (Phase 3) | 27.5 MiB | 11.46 GiB | Exact (`62555d7e…`, 28,737,558 rows) |
| `hacker-news` | `sort-number` | 170.205 | 128.599 | 1.3× (Phase 3) | 59.5 MiB | 24.37 GiB | Exact (`b7462bd3…`, 28,737,558 rows) |
| `hacker-news` | `duplicates` | 148.002 | 112.048 | 1.3× (Phase 3) | 55.0 MiB | 25.60 GiB | — |
| `hacker-news` | `duplicates-remove` | 353.288 | 108.958 | 3.2× (Phase 3) | 55.0 MiB | 25.59 GiB | Exact (`165d9336…`, 28,737,558 rows) |
| `hacker-news` | `sort-cancel` | 23.652 | 15.973 | 1.5× | 51.8 MiB | 56.9 MiB | — |

\*\*In the sequential 30-workload run, `weather/sort-number`, `weather/duplicates`, and
`weather/duplicates-remove` wrote ~90 GiB of temporary spill files before `hacker-news/open` ran,
evicting part of `hacknernews.csv` from the OS page cache (`4.226 s` at `2.80 GiB/s` NVMe read rate).
Immediately afterward with `hacknernews.csv` warm in page cache, `open --no-index-cache --threads 8`
completed in **0.472 s** (`25.12 GiB/s`, **16.6×** faster than Phase 1).

## Key Observations

- Every Phase 2 target (`open`, `viewport`, `find-early`, `find-absent`, `filter-numeric`,
  `filter-and`, `export`, sidecar hit + row seek, cancellation latency, and `C04` 10M-row warm CLI
  open) passed its Phase 2.7 gate by wide margins while keeping peak RSS bounded (`<= 24.95 MiB`
  indexing, `<= 46.1 MiB` parallel find/filter/export, `<= 147.4 MiB` across all 30 workloads).
- `replace-all`, `split-auto`, `combine`, `sort-number`, `duplicates`, and `duplicates-remove`
  remain single-threaded in Phase 2 (parallel out-of-core execution is scheduled for Phase 3), and
  all 14 output-producing workloads matched Phase 1 golden SHA-256 digests 100%.
- Zero third-party dependencies were added across all of Phase 2 (`Cargo.lock` unchanged since
  Phase 1 `178cabb`).
