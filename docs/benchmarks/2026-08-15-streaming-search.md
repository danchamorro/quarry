# Streaming literal search — 2026-08-15

## Decision

Ship the first bounded search slice: literal, case-sensitive **Find Next** over
decoded cells, with a background worker, progress, cancellation, and direct
row-and-column reveal. Keep regex, fuzzy search, all-match highlighting, and a
results panel deferred until the measured sequential path needs them.

The release benchmark scanned the complete 11.33 GiB reference file in
44.491 seconds at 260.82 MiB/s. Peak process RSS was 3.95 MiB, compared with
4.03 MiB for the 953.67 MiB deterministic file, so memory did not grow with
file size in this comparison.

## Implementation

- Search reuses `RecordScanner` and `parse_record`; quoted delimiters, doubled
  quotes, and embedded newlines follow the same parser as viewport reads.
- One worker reads 1 MiB chunks, retains at most one record and one match, and
  rejects records above the existing 64 MiB record limit.
- The structural index locates the starting checkpoint. Headers are skipped,
  and repeated Find Next requests resume at the next cell without wrapping.
- Dropping a search job requests cancellation and synchronously joins its
  worker. The UI also exposes progress and explicit cancellation.
- A match navigates to its physical record and shifts the bounded 32-column
  window when necessary. **First columns** restores columns 1–32.

Memory is bounded with respect to file size. It still depends on the query and
the largest individual record and its decoded fields, which is why the record
limit remains part of the contract.

## Environment

- Apple M3 Max MacBook Pro, 16 CPU cores, 128 GB RAM
- macOS 26.6.1 (25G76), arm64
- Rust 1.88.0
- working-tree release build based on commit `0e1be1d`
- fresh process for every recorded command
- search cache declared `unknown`; every search follows a full indexing prepass

## Datasets

| Dataset | Exact bytes | Records including header | SHA-256 |
|---|---:|---:|---|
| Deterministic 1 GB, seed 1, 11 columns | 1,000,000,077 | 5,117,758 | `afb0373394c884797ed77ec4a0fb915da0e0048c1161e205367a042745dea9c2` |
| `LARGE_FILE.csv` reference | 12,167,847,982 | 117,168,830 | `16b0469882c0ebf57f1b856144134ea60cc543f5394b246a97b5df721a4371f9` |

The deterministic generator places a doubled-quote cell and a quoted
multiline cell in data row 1. The full-file probe uses a synthetic uppercase
query that is absent from both recorded datasets.

## Reproduction

```bash
cargo build --workspace --release --locked

target/release/quarry generate --size 1GB --columns 11 --delimiter , \
  --output fixtures/generated/search-1gb.csv --seed 1

shasum -a 256 fixtures/generated/search-1gb.csv LARGE_FILE.csv

target/release/quarry-bench search fixtures/generated/search-1gb.csv \
  --query 'with "quotes"' --cache-state unknown

target/release/quarry-bench search fixtures/generated/search-1gb.csv \
  --query $'line one\nline two' --cache-state unknown

target/release/quarry-bench search fixtures/generated/search-1gb.csv \
  --query QUARRY_NO_MATCH_9F7B2C --cache-state unknown

target/release/quarry-bench search fixtures/generated/search-1gb.csv \
  --query QUARRY_NO_MATCH_9F7B2C \
  --cancel-after-bytes 67108864 --cache-state unknown

target/release/quarry-bench search LARGE_FILE.csv \
  --query QUARRY_NO_MATCH_9F7B2C --cache-state unknown

target/release/quarry-bench search LARGE_FILE.csv \
  --query QUARRY_NO_MATCH_9F7B2C \
  --cancel-after-bytes 67108864 --cache-state unknown
```

## Correctness and first-match results

| Query | Expected match | Actual match | Index prepass | Time to first match | Peak RSS |
|---|---|---|---:|---:|---:|
| `with "quotes"` | data row 1, column 1 | data row 1, column 1, offset 101 | 0.990 s | 0.001 s | 5.03 MiB |
| `line one\nline two` | data row 1, column 2 | data row 1, column 2, offset 101 | 0.978 s | 0.001 s | 3.98 MiB |

The automated regression also proves that raw doubled-quote syntax does not
match the decoded cell, that matching is case-sensitive, and that a search
crossing the 1 MiB chunk boundary still finds the quoted multiline record.

## Complete absent scans

| Dataset | Outcome | Exact bytes scanned | Records framed | Search time | Throughput | Peak RSS |
|---|---|---:|---:|---:|---:|---:|
| Deterministic 1 GB | Not found | 1,000,000,077 of 1,000,000,077 | 5,117,758 | 1.730 s | 551.35 MiB/s | 4.03 MiB |
| 12 GB reference | Not found | 12,167,847,982 of 12,167,847,982 | 117,168,830 | 44.491 s | 260.82 MiB/s | 3.95 MiB |

The 12 GB dataset is 12.17 times larger, while measured peak RSS was 0.08 MiB
lower. The different throughput reflects different record contents as well as
the declared unknown cache state; this report establishes the baseline rather
than claiming equal throughput across datasets.

## Cancellation

| Dataset | Requested | Final bytes | Search elapsed | Poll-inclusive cancellation latency | Outcome | Peak RSS |
|---|---:|---:|---:|---:|---|---:|
| Deterministic 1 GB | 64 MiB | 65 MiB | 0.117 s | 1.266 ms | Cancelled | 4.02 MiB |
| 12 GB reference | 64 MiB | 65 MiB | 0.251 s | 2.534 ms | Cancelled | 3.95 MiB |

The one-chunk overshoot is expected because the coordinator observes progress
between worker publications. The coordinator polls every 1 ms, so each latency
also includes the final polling and scheduler observation delay. Both workers
stopped far before EOF.

## Acceptance

The implementation passes the predeclared gates:

- decoded quoted and multiline probes match the exact data row and column;
- both row-1 probes complete below 100 ms after the index prepass;
- absent scans report Not found only after exact EOF byte and record counts;
- both poll-inclusive cancellation measurements remain below 100 ms, and both
  runs stop before EOF;
- both peak RSS measurements remain below 500 MiB;
- 12 GB peak RSS does not exceed 1 GB peak RSS by more than 32 MiB.

No wall-clock or RSS assertion was added to CI. Deterministic tests own parser,
cursor, cancellation, lifecycle, and reveal correctness; release measurements
own timing and memory evidence.

## Limits

- Search begins after structural indexing completes in this first UI slice.
- Search is literal, case-sensitive, sequential, and does not wrap.
- Only one match is retained; there is no all-results collection.
- Match progress is chunk-granular. A row-1 hit can report the remaining
  records framed in its 1 MiB read, so those counters are not the exact distance
  to the match.
- Current RSS was unavailable in the restricted benchmark shell; peak RSS came
  from the process `getrusage` measurement already used by Quarry benchmarks.
