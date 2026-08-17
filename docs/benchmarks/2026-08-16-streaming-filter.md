# Bounded streaming filter validation: 2026-08-16

## Decision

Ship the first Phase 4 filter slice: one literal, case-sensitive
contains or equality predicate over one decoded source column. A background
worker scans sequentially and builds an adaptive match-checkpoint index under a
fixed memory budget. Bounded filtered row reads rescan from the nearest match
checkpoint instead of retaining every matching row.

The match-heavy 12 GB run found 100,295,554 rows while the final filter index
used 8.97 MiB. It completed in 42.779 seconds at 271.26 MiB/s with 25.62 MiB
peak process RSS. The absent 12 GB scan completed in 41.387 seconds at
280.39 MiB/s with 3.92 MiB peak RSS.

At the time of this original slice, multiple predicates, case-insensitive
matching, regex, a full results panel, and streaming export remained deferred.

This report preserves the original single-predicate measurements. Quarry now
routes that command through the compatible `FilterQuery::single` path. See the
[multiple-predicate filter validation](2026-08-16-multiple-predicate-filter.md)
for the later AND-combined implementation and measurements.

## Implementation

- The worker reuses the shared record scanner and decoded-field parser, reads
  fixed 1 MiB chunks, and rejects records over the existing 64 MiB limit.
- The filter index owns its source column, operator, and literal value. It keeps
  the exact match count while compacting sparse match checkpoints whenever its
  16 MiB budget fills.
- Core can serve bounded filtered rows synchronously. An egui row request starts
  a cancellable background read at the nearest checkpoint, reevaluates the same
  predicate, and materializes only the requested matching rows. Rapid
  navigation cancels obsolete reads, keeps only the newest pending match window,
  and joins a cancelled read after it finishes. Lifecycle resets cancel and
  detach an active read-only worker so the render thread never waits for
  cleanup.
- The filter-index job reports exact bytes, physical records, and matches found.
  It supports snapshots, cancellation, wait, and cancel-and-join on drop.
- The CLI accepts a one-based source column, waits for the filter worker, and
  reads at most three 100-row samples for first, middle, final, and checksum
  evidence. It does not retain a full match list.
- Filtering starts directly from the source file and does not require a
  structural-index prepass.

## Environment

- Apple M3 Max MacBook Pro, 16 CPU cores, 128 GB RAM
- macOS 26.6.1 (25G76), arm64
- Rust 1.88.0
- working-tree release build from `codex/streaming-filter` with the workspace
  lockfile
- fresh process for each recorded command
- cache state declared `unknown` unless a run says otherwise

## Datasets

| Dataset | Exact bytes | Physical records including header | SHA-256 |
|---|---:|---:|---|
| Deterministic 1 GB, seed 1, 11 columns | 1,000,000,077 | 5,117,758 | `afb0373394c884797ed77ec4a0fb915da0e0048c1161e205367a042745dea9c2` |
| `LARGE_FILE.csv` reference | 12,167,847,982 | 117,168,830 | `16b0469882c0ebf57f1b856144134ea60cc543f5394b246a97b5df721a4371f9` |

The deterministic generator places a decoded doubled-quote value in column 1
every 97 data rows and a quoted multiline value in column 2 every 211 data
rows. The complete absent probe uses an uppercase value that is absent from the
recorded datasets. The 12 GB match-heavy probe uses `0` in column 1 to force
adaptive checkpoint compaction without recording source values.

## Reproduction

```bash
cargo build --workspace --release --locked

target/release/quarry generate --size 1GB --columns 11 --delimiter , \
  --output fixtures/generated/search-1gb.csv --seed 1

shasum -a 256 fixtures/generated/search-1gb.csv LARGE_FILE.csv

target/release/quarry-bench filter fixtures/generated/search-1gb.csv \
  --column 1 --operator contains --value 'with "quotes"' \
  --cache-state unknown

target/release/quarry-bench filter fixtures/generated/search-1gb.csv \
  --column 2 --operator equals --value $'line one\nline two' \
  --cache-state unknown

target/release/quarry-bench filter fixtures/generated/search-1gb.csv \
  --column 1 --operator contains --value QUARRY_NO_MATCH_9F7B2C \
  --cache-state unknown

target/release/quarry-bench filter fixtures/generated/search-1gb.csv \
  --column 1 --operator contains --value QUARRY_NO_MATCH_9F7B2C \
  --cancel-after-bytes 67108864 --cache-state unknown

target/release/quarry-bench filter LARGE_FILE.csv \
  --column 1 --operator contains --value 0 --cache-state unknown

target/release/quarry-bench filter LARGE_FILE.csv \
  --column 1 --operator contains --value QUARRY_NO_MATCH_9F7B2C \
  --cache-state unknown

target/release/quarry-bench filter LARGE_FILE.csv \
  --column 1 --operator contains --value QUARRY_NO_MATCH_9F7B2C \
  --cancel-after-bytes 67108864 --cache-state unknown
```

## Recorded results

### Correctness and bounded reads

| Predicate | Expected matches | Actual matches | First data row | Last data row | Filter time | Filter-index memory | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|
| Column 1 contains `with "quotes"` | 52,761 | 52,761 | 1 | 5,117,721 | 1.891 s | 1.21 MiB | 6.59 MiB |
| Column 2 equals `line one\nline two` | 24,255 | 24,255 | 1 | 5,117,595 | 1.852 s | 568.48 KiB | 4.14 MiB |

| Predicate | Sample rows | Bounded read time | Sample checksum |
|---|---:|---:|---:|
| Column 1 contains `with "quotes"` | 300 | 10.954 ms | `4595678385318792851` |
| Column 2 equals `line one\nline two` | 300 | 21.657 ms | `8858820499134049646` |

Both predicates matched decoded values rather than raw CSV syntax. Their exact
counts follow the generator's every-97-row and every-211-row schedules.

### Match-heavy adaptive-index scan

| Dataset and predicate | Matches | Exact bytes scanned | Physical records scanned | Filter time | Throughput | Filter-index memory | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|
| 12 GB, column 1 contains `0` | 100,295,554 | 12,167,847,982 | 117,168,830 | 42.779 s | 271.26 MiB/s | 8.97 MiB | 25.62 MiB |

The first match was data row 1 and the last was data row 117,168,827. Three
bounded 100-row samples completed in 4.286 ms with checksum
`6210562018458338161`. Match count grew beyond 100 million while the retained
checkpoint index stayed below its 16 MiB budget.

### Complete absent scans

| Dataset | Expected matches | Actual matches | Exact bytes scanned | Physical records scanned | Filter time | Throughput | Filter-index memory | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Deterministic 1 GB | 0 | 0 | 1,000,000,077 | 5,117,758 | 1.695 s | 562.66 MiB/s | 0 B | 3.95 MiB |
| 12 GB reference | 0 | 0 | 12,167,847,982 | 117,168,830 | 41.387 s | 280.39 MiB/s | 0 B | 3.92 MiB |

The 12 GB source is 12.17 times larger, while peak RSS for the same absent
predicate was 0.03 MiB lower. Throughput differs because the datasets contain
different record shapes and the OS cache state was not controlled.

### Cancellation

| Dataset | Requested | Final bytes | Filter elapsed | Poll-inclusive cancellation latency | Matches before cancellation | Outcome | Peak RSS |
|---|---:|---:|---:|---:|---:|---|---:|
| Deterministic 1 GB | 64 MiB | 65 MiB | 0.123 s | 1.283 ms | 0 | Cancelled | 3.95 MiB |
| 12 GB reference | 64 MiB | 65 MiB | 0.260 s | 2.533 ms | 0 | Cancelled | 3.92 MiB |

The one-chunk overshoot is expected because progress publishes after each 1 MiB
read. The CLI rejects a cancellation benchmark if the worker reaches EOF, so
neither result can be a raced complete scan labelled as cancellation.

## Automated coverage

- `cargo test -p quarry-core` passed 25 tests, including decoded quoted and
  multiline predicates, equals-empty semantics, ragged rows, exact incremental
  range reads, adaptive compaction, readable partial indexes after
  cancellation, background sparse-range reads, mid-gap cancellation, oversized
  records, and cancel-and-join on drop.
- `cargo test -p quarry-cli` passed five tests, including argument validation,
  completed contains/equality/empty-equality scans, bounded samples, threshold
  rejection, and a deterministic successful cancellation branch.
- The 26 egui regressions include labelled multiline filter controls, bounded
  match-only grid navigation, source-column identity, stale snapshot refresh,
  latest-request-wins background reads, and cancel/clear/reopen lifecycle
  behavior.
- The full workspace passed 63 tests, formatting, strict Clippy with warnings
  denied, and a locked release build.

## Viewer smoke test

Computer Use exercised the release egui app with a 789-byte, 60-data-row,
three-column fixture whose `state` column alternated between `TX` and `CA` and
whose `note` column included one quoted multiline value. Its SHA-256 was
`ba8256ccf44b210f762a1b317a661c672a68d1493db7031bbe4475d2b2e51680`.

The filter panel exposed the labelled file-column, operator, and multiline
value controls. Applying file column 2 contains `TX` produced exactly 30
matches and changed the grid to `Matches 1–26 of 30`, with only the odd source
rows visible. Page Down moved to `Matches 5–30 of 30`; Page Up returned to
`Matches 1–26 of 30`. Clear Filter restored `Rows 1–26` and removed the active
match count. Text, progress, controls, and the match-only grid remained legible
throughout the interaction.

## Acceptance gates

- decoded quote and multiline predicates return the exact deterministic match
  counts and first data row;
- complete absent scans reach exact EOF byte and physical-record counts before
  reporting zero matches;
- bounded reads return ordered first, middle, and final match samples without
  retaining the full result set;
- both poll-inclusive cancellation measurements remain below 100 ms and stop
  before EOF;
- the deterministic 1 GB and 12 GB absent-scan peak RSS measurements remain
  below 500 MiB;
- the 12 GB absent-scan peak RSS does not exceed the deterministic 1 GB
  absent-scan peak RSS by more than 32 MiB;
- the adaptive filter index remains within its configured memory budget.

All listed gates passed. The match-heavy 12 GB peak is higher than the
absent-run peak because checkpoint construction and compaction do real bounded
work, but it remains well below the 500 MiB product target.

No wall-clock or RSS assertion belongs in CI. Deterministic tests own predicate,
parser, compaction, range-read, progress, cancellation, and lifecycle
correctness. Release measurements own timing and memory evidence.

## Limits

- The first slice supports one source column and one predicate.
- Matching is literal and case-sensitive.
- Filter construction is a sequential full scan.
- Filtered reads may rescan records between retained match checkpoints.
- The filter index is process-local and is not persisted.
- Filtered export is not part of this validation.
- Current RSS was unavailable in the restricted benchmark shell; peak RSS came
  from the process `getrusage` measurement used by the other Quarry benchmarks.
