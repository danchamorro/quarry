# 50 GB capability benchmark: 2026-08-22

## Summary

Quarry completed a sequential release benchmark across its major large-file
workflows on a private 48.25 GiB delimited file with 225,437,755 data rows and
16 columns. The suite covered progressive opening, complete indexing, random
viewport reads, early-hit and complete-scan Find, single- and multi-predicate
filtering, filtered export, sparse editing, Replace All, automatic Split,
Combine, and stable Sort.

The first 100 rows were available in 3.085 ms. A complete structural index was
built in 72.957 seconds at 677.23 MiB/s with 5.05 MiB peak process RSS. After
indexing, deterministic random 100-row viewport reads had 1.263 ms median and
1.570 ms p95 latency.

Every successful write left the source size unchanged and published only a
complete destination. Full-size benchmark outputs were created one at a time,
validated where noted, and removed before the next write-heavy run. No source
rows or person-level field values are included in this report.

## Environment

| Item | Value |
|---|---|
| Machine | MacBook Pro, Apple M3 Max, 16 cores |
| Memory | 128 GB |
| Operating system | macOS 26.6.1 (25G76) |
| Build | Cargo release profile |
| Source | Base commit `da25fb336204`, plus the benchmark-harness additions in this change |
| Storage | Source, temporary files, and destinations on the local APFS Data volume |
| Initial free space | 703.74 GiB |
| Cache sequence | First open labeled `unknown`; later runs labeled `warm` |

macOS file-cache eviction was not controlled, so these results are not a
cold-cache comparison. Current RSS was unavailable from the restricted
benchmark process. Peak RSS came from the same `getrusage` path used by the
existing Quarry benchmarks.

## Fixture

| Property | Value |
|---|---:|
| File | `LARGE_FILE_50GB.csv` |
| Exact bytes | 51,809,121,923 |
| Binary size | 48.25 GiB |
| Data rows | 225,437,755 |
| Header rows | 1 |
| Columns | 16 |
| Delimiter | Pipe |
| Line endings | CRLF |

The fixture is private, ignored by Git, and not described as synthetic. Its
content values are intentionally omitted.

## Results

### Open, index, and navigation

| Measurement | Result |
|---|---:|
| File open | 0.025 ms |
| First 100 rows | 3.085 ms |
| Bootstrap bytes read | 1.00 MiB |
| Complete index | 72.957 s |
| Index throughput | 677.23 MiB/s |
| Index memory | 859.98 KiB |
| Peak process RSS | 5.05 MiB |

The viewport benchmark rebuilt a warm index in 63.187 seconds, then performed
500 reads for each access pattern with 100 rows per request:

| Pattern | Minimum | Median | p95 | Maximum | Requests/s |
|---|---:|---:|---:|---:|---:|
| Repeated | 1.120 ms | 1.187 ms | 1.301 ms | 2.294 ms | 834.8 |
| Sequential | 1.115 ms | 1.194 ms | 1.292 ms | 1.483 ms | 833.4 |
| Random | 1.158 ms | 1.263 ms | 1.570 ms | 3.542 ms | 742.9 |

Peak process RSS for the index and viewport process was 5.41 MiB.

### Find

| Scenario | Index prepass | Search work | Result | Peak RSS |
|---|---:|---:|---|---:|
| Known early literal | 57.949 s | 0.001 s, 1.00 MiB and 4,560 rows scanned | Match | 4.16 MiB |
| Absent literal | 62.058 s | 121.591 s, entire 48.25 GiB scanned at 406.35 MiB/s | Not found | 5.05 MiB |

The search timer excludes the required index prepass and reports the literal
search itself. Find retains no full results list.

### Filtering and filtered export

| Scenario | Matches | Time | Throughput | Index memory | Peak RSS |
|---|---:|---:|---:|---:|---:|
| One absent equality predicate | 0 | 116.443 s | 424.32 MiB/s | 0 B | 4.08 MiB |
| Two equality predicates | 5,368,672 | 109.659 s | 450.57 MiB/s | 7.68 MiB | 24.53 MiB |

The two-predicate run also read 300 bounded sample rows in 6.720 ms after the
scan completed.

A zero-match filtered export scanned the complete file in 119.399 seconds at
413.82 MiB/s, published the 169-byte header and no data rows, left the source
size unchanged, and used 4.11 MiB peak process RSS. The zero-match case was
chosen to exercise the complete scan and publication path without retaining a
second file-sized result.

### Editing and Replace All

| Workflow | Worker result | Benchmark-only validation | Peak RSS |
|---|---|---:|---:|
| Save As with two sparse cell edits | 79.776 s at 619.35 MiB/s | 59.239 s, both cells read back | 5.16 MiB |
| Replace All | 1,044,664 replacements in 328.476 s at 150.42 MiB/s | Source size and complete destination publication checked | 4.23 MiB |

Sparse Save As wrote 51,809,121,949 bytes. Replace All used a same-length
replacement and wrote 51,809,121,923 bytes, exactly matching the source size.
Both operations reported an unchanged source and an atomically published
destination. A focused CLI regression independently verifies exact Replace
All output and cancellation cleanup on a small fixture.

### Split and Combine

| Workflow | Analysis | Rewrite | Benchmark-only validation | Peak RSS |
|---|---:|---:|---:|---:|
| Automatic Split | 114.789 s at 430.43 MiB/s | 340.719 s at 145.01 MiB/s | 119.500 s | 4.19 MiB |
| Combine two columns | Not required | 328.264 s at 150.52 MiB/s | 115.259 s | 4.20 MiB |

Automatic Split scanned all 225,437,755 data rows to discover two output
columns before rewriting the file. Both transformations validated the output
header, output-column count, exact data-row count, and transformed first,
middle, and final rows. Source size remained unchanged.

The user-visible automatic Split work was 455.508 seconds for analysis plus
rewrite. Its 119.500-second validation pass is benchmark evidence, not extra
application work. Combine's 115.259-second validation pass is likewise outside
its 328.264-second application operation.

### Stable Sort

| Measurement | Result |
|---|---:|
| Data rows sorted | 225,437,755 |
| Sort wall time | 1,113.443 s (18 min 33.443 s) |
| Benchmark-only validation | 484.747 s (8 min 4.747 s) |
| Complete benchmark | 1,598.190 s (26 min 38.190 s) |
| Sorted runs | 13,998 |
| Merge passes | 13 |
| Estimated temporary disk | 204.76 GiB |
| Measured peak temporary disk | 102.91 GiB |
| Peak process RSS | 19.38 MiB |

Sort preserved the exact 225,437,755-row data count and exact 169-byte raw
header. Completion passed a complete order scan, bounded dual record-multiset
fingerprints, and exact stable equal-key source-ordinal checks. The source size
remained unchanged. The destination used owner-only `0600` permissions and was
published only after the guarded sort completed.

## Reproduction

Build once so compile time is excluded:

```bash
cargo build --release --locked -p quarry-cli --bin quarry-bench
```

Use an absent, private output directory on the same volume. These commands show
the measured operation shapes. Private fixture literals are represented by
placeholders where publishing them is unnecessary.

```bash
target/release/quarry-bench open LARGE_FILE_50GB.csv \
  --rows 100 --metrics-only --cache-state unknown

target/release/quarry-bench viewport LARGE_FILE_50GB.csv \
  --iterations 500 --rows 100 --seed 20260822 --cache-state warm

target/release/quarry-bench search LARGE_FILE_50GB.csv \
  --query QUARRY_NOT_PRESENT_20260822 --cache-state warm

target/release/quarry-bench filter LARGE_FILE_50GB.csv \
  --column 10 --operator equals --value '<common-value>' \
  --and 15 equals '<flag-value>' --cache-state warm

target/release/quarry-bench export LARGE_FILE_50GB.csv \
  --output <output-dir>/filtered.csv \
  --column 3 --operator equals --value QUARRY_NOT_PRESENT_20260822 \
  --cache-state warm

target/release/quarry-bench edit-save-as LARGE_FILE_50GB.csv \
  --output <output-dir>/edited.csv \
  --edit 1 3 QUARRY_BENCH_FIRST \
  --edit 112718877 3 QUARRY_BENCH_MIDDLE \
  --cache-state warm

target/release/quarry-bench replace-all-save-as LARGE_FILE_50GB.csv \
  --output <output-dir>/replaced.csv \
  --query '<known-repeated-literal>' \
  --replacement '<same-length-replacement>' \
  --cache-state warm

target/release/quarry-bench transform-save-as LARGE_FILE_50GB.csv \
  --output <output-dir>/split.csv --split-auto 1 : \
  --cache-state warm

target/release/quarry-bench transform-save-as LARGE_FILE_50GB.csv \
  --output <output-dir>/combined.csv \
  --join 3,5 ' ' --output-header FULL_NAME \
  --cache-state warm

target/release/quarry-bench sort-save-as LARGE_FILE_50GB.csv \
  <output-dir>/sorted.csv \
  --column 5 --order asc --header first-row --cache-state warm
```

Destinations must not already exist. Full-output commands were run one at a
time, checked, and cleaned before the next command. Before Sort, the Data volume
had 698 GiB free, above the 204.76 GiB conservative allowance.

## What is not a 50 GB throughput benchmark

View-only column hide, show, and reorder, column selection, auto-fit, bounded
copy, and Undo/Redo do not scan all 50 GB. Their useful measurements are
interaction latency, bounded viewport work, and correctness, which are covered
by focused regressions and the existing UI validation reports.

Move, Delete, and header rename use the same bounded streaming Save As worker
measured by sparse editing and structural transformations. They retain exact
regression coverage for ordering, ragged rows, sparse overlays, source
preservation, and cancellation, but this suite does not invent separate
benchmark-only commands for the same persistence path.

## Limits of this evidence

- The fixture is private and not reproducibly generated from a public seed.
- Cache states describe the observed sequence; no controlled cold-cache run
  was performed.
- Split and Combine validation samples the first, middle, and final transformed
  rows after confirming exact record counts. It is not a decoded comparison of
  every output row.
- Replace All relies on the production worker's replacement count and
  publication summary plus exact focused regressions. This run did not perform
  a second complete content scan of the 48.25 GiB destination.
- Sort validation is stronger and includes a complete order scan, bounded
  record-multiset evidence, and exact stable-tie evidence.

## Result

The 50 GB capability gate passes. Quarry exercised every major full-file engine
path with bounded process memory, preserved the source, published no partial
destination, and completed the stable sort within the preflight disk bound.
