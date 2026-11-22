# Phase 1 Gate 1A Public Dataset Baseline: 2026-11-21

This report freezes the Phase 1 Gate 1A single-threaded CLI baseline and golden output
digests for Quarry's two official public CSV datasets (`daily_weather_preprocessed_1896_2023.csv`
and `hacknernews.csv`) prior to Phase 2 SIMD/parallel indexing and Phase 3 multi-core operations.
Machine-readable golden digests compatible with `scripts/benchmark-public-datasets.py --expected-outputs`
are stored in [2026-11-21-phase1-public-datasets-golden.json](2026-11-21-phase1-public-datasets-golden.json).

## Environment & Provenance

| Item | Value |
|---|---|
| Venue | `host-bare-metal` (headless CLI only) |
| Machine | MacBook Pro, Apple M3 Max, 16 cores, 128 GiB RAM |
| Operating system | macOS 27.0.1 (`26A434`), `arm64` |
| Rust | `rustc 1.88.0` (`aarch64-apple-darwin`) |
| Engine revision | `178cabbf5973ee112d4358f37544055fa9b4327c` (`clean`) |
| Executable SHA-256 | `4d3736ec145daf46274f167af82f5945f369b9dde4cd6dc1d8f10c324715a78d` |
| Cache protocol | `warm` — Full SHA-256 read before measurements; warm label only, no OS cache eviction or control. |
| Started / Finished (UTC) | `2026-11-21T23:11:33.941662+00:00` -> `2026-11-22T00:19:07.214972+00:00` |
| Sources unchanged | `True` |

## Verified Inputs

| Dataset | File | Bytes | SHA-256 |
|---|---|---|---|
| `weather` | `daily_weather_preprocessed_1896_2023.csv` | 9,770,967,575 | `b775c8b7e6d39643bc1e10874deecc9b067e2866c79a8e647a6c93550412d47f` |
| `hacker-news` | `hacknernews.csv` | 12,718,039,463 | `165d9336b6266519da4e99d0348576537f80337e40d1794aefb00258a49a0373` |

## Workload Timings, Peak Memory, Peak Scratch & Output Digests

| Dataset | Case | Command (s) | Peak RSS | Peak Scratch | Equality | Output Rows | Output Bytes | Output SHA-256 |
|---|---|---:|---:|---:|---|---:|---:|---|
| `weather` | `open` | 37.936 | 5.4 MiB | 0 | — | — | — | — |
| `weather` | `viewport` | 45.543 | 6.2 MiB | 0 | — | — | — | — |
| `weather` | `find-early` | 38.791 | 4.9 MiB | 0 | — | — | — | — |
| `weather` | `find-absent` | 110.366 | 5.4 MiB | 0 | — | — | — | — |
| `weather` | `filter-numeric` | 70.412 | 27.7 MiB | 0 | — | — | — | — |
| `weather` | `filter-and` | 68.644 | 6.7 MiB | 0 | — | — | — | — |
| `weather` | `export` | 69.330 | 5.0 MiB | 2.1 MiB | `raw-byte` | 26,506 | 2,238,131 | `4cc7682452d1e221c56d4bdefa7255b2b99b67bea9bd53c702338dc6f98acb6a` |
| `weather` | `edit-save-as` | 84.779 | 7.2 MiB | 9.10 GiB | `decoded-record` | 131,985,330 | 9,770,967,599 | `1dda7240efc1192b9b6551d384c2701298b74bd78d50fdb4f8c9a33fb338aa09` |
| `weather` | `replace-all` | 187.803 | 6.2 MiB | 9.10 GiB | `decoded-record` | 131,985,330 | 9,770,967,575 | `99df9f38bb93fecec9f4c42e859bef8933072de16df6d6e086454c0fdc14a586` |
| `weather` | `split-auto` | 335.654 | 7.3 MiB | 9.10 GiB | `decoded-record` | 131,985,330 | 9,775,630,595 | `a3473c044b340e49506dd811ecc08801897655d2a536735ed115ec2238b373da` |
| `weather` | `combine` | 271.298 | 7.2 MiB | 9.10 GiB | `decoded-record` | 131,985,330 | 9,770,967,561 | `c5f57f5d7431720fb52d4bb07e274d317baab592f99035c6ce1f10e9da1551b0` |
| `weather` | `sort-number` | 805.252 | 81.8 MiB | 21.98 GiB | `raw-byte` | 131,985,330 | 9,770,967,575 | `4be7c66945d56ececb1564c08ab94f470b992db8cbdedd49f4e18ceeb216d58b` |
| `weather` | `duplicates` | 271.080 | 141.4 MiB | 33.03 GiB | — | — | — | — |
| `weather` | `duplicates-remove` | 264.782 | 141.4 MiB | 33.04 GiB | `raw-byte` | 127,261,910 | 9,477,642,738 | `10e09e36fbd11a1b5742aa3938fcfff71066b45cf2bae7d461a17bba5e921100` |
| `weather` | `sort-cancel` | 45.611 | 42.0 MiB | 80.0 MiB | — | — | — | — |
| `hacker-news` | `open` | 7.812 | 5.0 MiB | 0 | — | — | — | — |
| `hacker-news` | `viewport` | 8.815 | 7.6 MiB | 0 | — | — | — | — |
| `hacker-news` | `find-early` | 7.226 | 6.0 MiB | 0 | — | — | — | — |
| `hacker-news` | `find-absent` | 22.118 | 7.7 MiB | 0 | — | — | — | — |
| `hacker-news` | `filter-numeric` | 14.360 | 13.7 MiB | 0 | — | — | — | — |
| `hacker-news` | `filter-and` | 15.221 | 14.6 MiB | 0 | — | — | — | — |
| `hacker-news` | `export` | 14.544 | 5.9 MiB | 51.6 MiB | `raw-byte` | 128,223 | 54,128,598 | `1ebb146861f71c371f13c48f757647f9175fe21af94ba6bb1e9a9b8a8c7d3c16` |
| `hacker-news` | `edit-save-as` | 21.492 | 7.4 MiB | 11.84 GiB | `decoded-record` | 28,737,558 | 12,718,039,455 | `e84956182bc15e1c5f9350f8405b55a63387f7da7a72ec2743f09f64cbb8ac10` |
| `hacker-news` | `replace-all` | 44.833 | 8.7 MiB | 11.84 GiB | `decoded-record` | 28,737,558 | 12,718,030,503 | `d4e78f74e1e6024a00c96f45f2ed316dc494654ba28c1cdf0dbed19d96da7181` |
| `hacker-news` | `split-auto` | 79.997 | 9.5 MiB | 11.46 GiB | `decoded-record` | 28,737,558 | 12,303,294,624 | `47e1d39560aa4059f5ca9bd4244ef15960efa988ffae9984e680415f7c3b4ac9` |
| `hacker-news` | `combine` | 67.062 | 9.1 MiB | 11.46 GiB | `decoded-record` | 28,737,558 | 12,303,294,624 | `62555d7ecc8bef65717b6f6036d39d4aec780727d427100552f0de435865b077` |
| `hacker-news` | `sort-number` | 170.205 | 39.4 MiB | 24.37 GiB | `raw-byte` | 28,737,558 | 12,718,039,463 | `b7462bd362f26ae1fbf433e7c7001370bafcc9f505163a44ef70c6f0aec9fd47` |
| `hacker-news` | `duplicates` | 148.002 | 55.7 MiB | 25.59 GiB | — | — | — | — |
| `hacker-news` | `duplicates-remove` | 353.288 | 55.2 MiB | 25.61 GiB | `raw-byte` | 28,737,558 | 12,718,039,463 | `165d9336b6266519da4e99d0348576537f80337e40d1794aefb00258a49a0373` |
| `hacker-news` | `sort-cancel` | 23.652 | 31.7 MiB | 64.0 MiB | — | — | — | — |

## Key Observations

- All 30/30 workloads exited `0` with verified unchanged source hashes and zero unterminated-quote outputs.
- Per-workload peak RSS remained strictly bounded across all 9.10 GiB and 11.84 GiB workloads (max `141.4 MiB` on `weather/duplicates` and `weather/duplicates-remove`; `<= 81.8 MiB` on sorting; `<= 27.7 MiB` on indexing, search, filter, and transforms).
- `hacker-news/duplicates-remove` on unique column 1 (`id`) produced a byte-for-byte identical output digest (`165d9336b6266519da4e99d0348576537f80337e40d1794aefb00258a49a0373`, `28,737,558` logical rows), confirming first-row retention and original order preservation on quoted multiline CSVs.
- `weather/duplicates-remove` on columns `1,2` reduced `131,985,330` rows (`9,770,967,575` bytes) to `127,261,910` rows (`9,477,642,738` bytes), removing `4,723,420` duplicate records.
- Note: these numbers are warm-cache single-threaded baselines captured under ordinary host background load (not compared directly against earlier historical runs or VM timings); Phase 2 and Phase 3 optimizations must match the frozen golden digests (`--expected-outputs docs/benchmarks/2026-11-21-phase1-public-datasets-golden.json`) while demonstrating matched-venue speedups.
