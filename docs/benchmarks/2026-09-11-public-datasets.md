# Public dataset capability benchmarks: 2026-09-11

Quarry completed 14 workloads on each of the two public CSVs. Number sorting
took 141.508 seconds for weather and 67.258 seconds for Hacker News. All completed
cases passed their CLI checks, and both original files retained identical
SHA-256 hashes. Two initial Combine setup errors and their successful corrected
reruns are preserved in the [machine-readable evidence](2026-09-11-public-datasets-results.json),
along with every command, log, timing, and the benchmark CLI patch.

## Inputs and reproduction

These measurements use Quarry's two [official public benchmark datasets](../BENCHMARK_DATASETS.md):
the Taiwan weather and Hacker News CSVs linked by ClickHouse. The download guide
includes the original sources, extraction commands, and checksum instructions.
The files were used with their original headers and contents.

| Property | Taiwan weather | Hacker News |
|---|---|---|
| CSV | `daily_weather_preprocessed_1896_2023.csv` | `hacknernews.csv` |
| Exact bytes | 9,770,967,575 | 12,718,039,463 |
| Binary size | 9.10 GiB | 11.84 GiB |
| Data rows, excluding header | 131,985,329 | 28,737,557 |
| Columns | 30 | 15 |
| Delimiter | Comma | Comma |
| Header | First record | First record |
| SHA-256 | `b775c8b7e6d39643bc1e10874deecc9b067e2866c79a8e647a6c93550412d47f` | `165d9336b6266519da4e99d0348576537f80337e40d1794aefb00258a49a0373` |

The extracted weather file also matches the upstream MD5,
`1132248c78195c43d93f843753881754`. The Hacker News filename preserves
the upstream spelling, `hacknernews`.

File size alone does not describe the work involved. Weather has about 4.6 times
as many records and twice as many columns as Hacker News. These are distinct
workloads, not a controlled comparison of scaling by bytes.

From the repository root:

```bash
cargo build --release --locked -p quarry-cli --bin quarry-bench
python3 scripts/benchmark-public-datasets.py \
  /absolute/path/quarry-benchmark-data \
  /absolute/path/new-quarry-benchmark-results
```

The results directory must be new. The runner freezes the built executable,
records every exact command and exit status in `results.json`, and captures one
log per case. It checks input metadata after every case and SHA-256 before and
after the suite. Successful generated outputs are removed after validation;
failed cases retain their output directories for inspection.

## Environment and timing boundaries

| Item | Value |
|---|---|
| Machine | MacBook Pro, Apple M3 Max, 16 cores (12 performance, 4 efficiency) |
| Memory | 128 GiB |
| Operating system | macOS 26.6.2 (25G83), arm64 |
| Rust | 1.88.0, `aarch64-apple-darwin` |
| Build | Cargo release profile, locked dependencies |
| Engine revision | `1ce1b342ddfe1b7d24606f190cfc0a5048b4d6ce` |
| Harness | That revision plus this change's CLI timing and index-adoption measurements; engine code unchanged |
| Executable SHA-256 | `b599039e6a70121e90b468dd80b29130436855a6979155d4bc15dea4a356051f` |
| Storage | Inputs, working files, and outputs on the local APFS Data volume |
| Initial available space | 835,943,071,744 bytes |
| Cache label | Warm, following complete SHA-256 input reads |

The suite runs one command at a time. No additional large-data benchmark or Rust
build or test suite was started during measurements. Documentation work and a
small runner smoke check continued. macOS cache eviction and ordinary desktop
background load are not controlled.
Both files fit in this machine's physical memory. These results do not establish
cold-cache performance or larger-than-RAM behavior.

Operation timers report work inside the corresponding engine worker. Whole
command time includes source indexing, setup, and any independent benchmark
validation and hashing. First-row and viewport measurements are CLI parsing and
read latency, not desktop first-paint or scrolling frame timings. Peak RSS is
process resident memory reported by `getrusage`, not total OS file-cache use.

The existing sort validator traverses the output in 1,000-row batches. Each
indexed read scans a larger buffer from a sparse checkpoint, so this exhaustive
check can take much longer than sorting itself. It builds a fresh independent
index even after testing completed-index adoption. A one-second process sample
was taken during weather sort validation to investigate this overhead, after
the sort worker had published its output. Validation time includes that diagnostic.

## Workloads

Each file receives the same operation shapes, using these public column values:

| Case | Taiwan weather | Hacker News |
|---|---|---|
| Open | First 100 rows, complete index | First 100 rows, complete index |
| Viewport | 500 requests per pattern, 100 rows, seed 20260911 | Same |
| Early Find | `C0X100` | `callmeed` |
| Absent Find | `QUARRY_NOT_PRESENT_20260911` | Same |
| Numeric filter | `RH` (5) between 70 and 80 inclusive | `score` (12) at least 100 |
| AND filter and export | Above plus `StationId` (1) equals `C0X100` | Above plus `type` (3) equals `story` |
| Sparse Save As | Change column 1 on data rows 1 and 100,000 | Same |
| Replace All | `C0X100` to `C0X101` | `ClickHouse` to `CLICKHOUSE` |
| Automatic Split | `MeasuredDate` (2) on a space | `time` (5) on a space |
| Combine | `StationId` and `MeasuredDate` (1,2), separator `\|` | `type` and `by` (3,4), separator `\|` |
| Number sort | `RH` (5), ascending | `score` (12), ascending |
| Duplicate analysis | `StationId,MeasuredDate` (1,2), case-sensitive | `id` (1), case-sensitive |
| Sort cancellation | Request after at least 64 MiB scanned | Same |

Find, Replace All, and filters use their default case-insensitive matching.
Sparse Save As writes `QUARRY_BENCH_FIRST` and `QUARRY_BENCH_LATER` into the two
chosen cells. Combine names the resulting column `Combined`. Sort and duplicate
analysis explicitly preserve the first record as the header. Cancellation is
polled, so its observed byte count can exceed
64 MiB. Duplicate analysis counts later occurrences and does not publish a
deduplicated file.

## Results

All 28 workloads passed after correcting the Combine command. Each successful
case was measured once. The recorded interval was 22:30:57–23:21:28 UTC
(50 minutes 31 seconds), including benchmark validation, post-run hashing, and
the Combine reruns. Downloads, compilation, and the initial checksum prepass
are outside that interval.

### Open and navigation

| Measurement | Taiwan weather | Hacker News |
|---|---:|---:|
| First 100 rows | 9.541 ms | 2.211 ms |
| Complete index | 34.556 s | 7.947 s |
| Index throughput | 269.66 MiB/s | 1.49 GiB/s |
| Index memory | 503.48 KiB | 109.64 KiB |
| Open/index peak RSS | 4.78 MiB | 4.39 MiB |
| Random 100-row viewport p50 | 3.832 ms | 0.754 ms |
| Random 100-row viewport p95 | 4.542 ms | 1.402 ms |
| Viewport process peak RSS | 6.06 MiB | 6.84 MiB |

The viewport command first built its own index, then ran 500 requests for each
of three patterns. Repeated-read p50/p95 was 4.275/4.985 ms for weather and
1.266/1.326 ms for Hacker News. Sequential-read p50/p95 was 3.990/4.642 ms and
0.730/1.337 ms respectively. These are distributions within one run, not
confidence intervals across repeated benchmark runs.

### Full-file workflows

All values in this table are seconds. **Engine work** excludes setup and later
benchmark validation. **Whole command** includes both. Split engine work is the
sum of its analysis and rewrite timers.

| Operation | Weather engine work | Hacker News engine work | Weather whole command | Hacker News whole command |
|---|---:|---:|---:|---:|
| Early Find | 0.004 | 0.001 | 34.662 | 7.456 |
| Absent Find, full scan | 73.141 | 15.194 | 108.015 | 22.566 |
| Numeric filter | 69.756 | 15.026 | 69.783 | 15.072 |
| AND filter | 70.293 | 15.095 | 70.322 | 15.143 |
| Filtered export | 70.583 | 15.302 | 70.599 | 15.309 |
| Two-edit Save As | 40.876 | 12.228 | 77.012 | 20.294 |
| Replace All | 192.466 | 46.887 | 192.483 | 46.895 |
| Automatic Split | 263.556 | 65.167 | 334.324 | 80.304 |
| Combine | 198.145 | 51.423 | 265.000 | 65.900 |
| Stable Number sort | 141.508 | 67.258 | 805.565 | 158.378 |
| Duplicate analysis | 264.752 | 100.949 | 264.864 | 101.031 |

Both early searches found a match in the first data row. Their whole-command
times include a complete source-indexing prepass: 34.644 seconds for weather,
7.449 seconds for Hacker News. Both absent searches scanned every byte and
returned not found.

| Result | Taiwan weather | Hacker News |
|---|---:|---:|
| Numeric filter matches | 12,680,122 | 129,507 |
| AND filter matches | 26,505 | 128,222 |
| Exported data rows | 26,505 | 128,222 |
| Exported bytes, including header | 2,238,131 | 54,128,598 |
| Replacements | 93,773 | 995 |
| Split output columns discovered | 2 | 2 |

The filter runs each read 300 bounded result samples. Export counts agreed with
the corresponding AND filters. Save As independently read back both edited
cells; its validation index and reads took 36.121 seconds for weather and
8.058 seconds for Hacker News.

Split analysis took 67.948/14.297 seconds (weather/Hacker News), followed by
195.608/50.870 seconds rewriting the files. Independent validation then took
70.751/15.126 seconds, checking the complete row count, header, output width,
and transformed first, middle, and final rows. Combine passed the same checks
in 66.839/14.471 seconds after its rewrite. CSV serialization can change
output size even for same-length replacements; output bytes are recorded in
the evidence rather than assumed equal to the source.

### Sort, completed-index reuse, and storage

| Measurement | Taiwan weather | Hacker News |
|---|---:|---:|
| Source indexing before sort | 34.332 s | 7.482 s |
| Additional sort preparation bytes scanned | 0 | 0 |
| Sort worker | 141.508 s | 67.258 s |
| Output open and completed-index adoption | 5.915 ms | 1.451 ms |
| Independent validation index and reads | 608.838 s | 55.885 s |
| Sorted runs | 852 | 830 |
| Merge passes | 2 | 2 |
| Conservative temporary-disk allowance | 83.12 GiB | 97.00 GiB |
| Measured peak temporary disk | 22.00 GiB | 24.40 GiB |
| Peak process RSS through sort | 85.58 MiB | 38.55 MiB |

Both sorts preserved every data row and the exact raw header (256 bytes for
weather, 115 for Hacker News). Both passed the complete ordering scan, bounded
dual record-multiset fingerprint checks, and stable equal-key source-ordinal
checks. Destinations had observed Unix permissions `0600`.

The completed indexes contained 131,985,330 and 28,737,558 records respectively,
including headers. Their counts and byte totals agreed with independently
rebuilt indexes, and first/middle/last data rows read through each index agreed.
The measured handoff covers opening and index adoption, not GUI rendering.
Zero preparation bytes applies to this already-indexed source path; sorting a
file whose initial index is unfinished is a different scenario.

### Duplicate analysis and cancellation

| Measurement | Taiwan weather | Hacker News |
|---|---:|---:|
| Later rows matching an earlier selected key | 4,723,420 | 0 |
| Rows retained in private candidate | 127,261,909 | 28,737,557 |
| Duplicate-analysis peak temporary disk | 33.05 GiB | 25.61 GiB |
| Peak process RSS through duplicate analysis | 148.23 MiB | 55.34 MiB |
| Cancellation request threshold | 64 MiB | 64 MiB |
| Observed bytes scanned before cancellation finished | 65 MiB | 65 MiB |
| Cancellation latency after request | 5.087 ms | 1.234 ms |
| Cancelled sort worker duration | 0.654 s | 0.186 s |
| Whole cancellation command | 46.308 s | 21.853 s |

Duplicate counts describe equality of the selected keys, not necessarily every
field in the record. Blank and missing keys match; whitespace is significant.
Both private analysis candidates were cleaned up without publishing a
destination. Both cancellation tests left no destination or temporary files in
their dedicated case directories. Whole cancellation-command times include
initial source indexing and final source hashing, outside cancellation latency.

The highest peak RSS reported across the workloads was 148.23 MiB for weather
and 55.34 MiB for Hacker News, both during duplicate analysis.

## Validation and evidence boundaries

The initial Combine commands were rejected before doing file work because they
omitted the required output header. The runner was corrected to pass
`--output-header Combined`; both cases passed when repeated after the original
queue using the same frozen release executable. The two rejected attempts are
retained as setup errors, not treated as application failures or timing results.

The initial queue's complete before/after SHA-256 checks matched for both input
files. Final hashes after the Combine reruns also matched those original
identities. The [evidence file](2026-09-11-public-datasets-results.json) retains
all 30 attempts, with 28 successful workloads and two rejected setup commands.
Only filesystem path prefixes are normalized in its logs and argument lists.

Validation uses the existing CLI checks: full sort ordering and row/header
preservation, transformed-output indexing plus three sample rows, edited-cell
read-back, export summary/count checks, and operation-specific cleanup checks.
Replace All reports the worker's count and published output; this run does not
independently compare every replacement against a second implementation.
These measurements exercise engine commands, not GUI interactions, Apple
distribution, every sort mode, or cancellation of every operation.

The updated harness passed all 30 CLI tests, strict CLI Clippy checks, formatting,
and the runner smoke check. The smoke check covers source preservation, logs,
successful-output cleanup, refusal to reuse a results directory, failure-artifact
retention, and the required Combine header:

```bash
cargo test --locked -p quarry-cli
cargo clippy --locked -p quarry-cli --all-targets -- -D warnings
cargo fmt -p quarry-cli -- --check
python3 scripts/test-benchmark-public-datasets.py
```
