# Split and join transformation validation: 2026-08-19

## Summary

This report validates the bounded persistence engine used to materialize Split
and Join operations. In the desktop workflow, users select numbered columns,
open **Split Columns…** or **Combine Columns…** from the header context menu,
and confirm a compact dialog with OK. Split derives its width from a cancellable
analysis of the current document. The confirmed operation is streamed into a
private working CSV and reopened as the ordinary editable grid, where users can
edit cells or headers and apply more operations. Cancel changes nothing. Save
atomically publishes the current working document, Save As publishes it to a
new path, and Discard restores the last opened or saved file. One-level
structural Undo and Redo move between adjacent document versions.

The measurements below deliberately isolate one deterministic operation per
command. They are persistence-engine evidence, not a limit on repeated desktop
operations or a description of the desktop interaction.

Release benchmarks applied Split and Join separately to deterministic 1 GB and
12 GB files. Each successful command built source and destination structural
indexes outside the Save As timer, confirmed equal record counts, validated the
semantic header and resulting schema, and compared the first, middle, and
final data rows through the same pure transformation helper used by the worker.
Deterministic destination hashes supplement those semantic checks.

On the final reviewed build, the 1 GB Split and Join completed in 5.069 and
5.226 seconds with 4.23 and 4.17 MiB peak RSS. The 12 GB Split and Join
completed in 65.314 and 63.289 seconds with 4.17 MiB peak RSS each. These
measurements are scoped to the two deterministic datasets and the exact
commands below.

Cancellation after the 64 MiB threshold stopped both dataset runs at 65 MiB
scanned, published no destination, removed temporary output, and left each
source hash unchanged.

## Persistence engine under test

- The timed benchmark commands use no sparse header or cell edits. Core
  regressions separately verify that existing sparse edits apply before an
  isolated structural operation.
- Split uses a non-empty literal separator, keeps the unsplit remainder in the
  final field, and pads rows with fewer parts using empty fields. The desktop
  first analyzes current data and sparse edits to derive the widest row, then
  materializes every row with that schema.
- Join reads at least two unique selected columns in document order, inserts
  the combined value at the leftmost selected position, and removes the
  selected originals. Its literal separator may be empty. The desktop keeps the
  leftmost selected current header for the joined result.
- Move and Delete later added `ColumnTransformation::Arrange` to this same
  materialization worker. Arrange retains unique known source columns in an
  explicit output order and appends any undiscovered trailing ragged fields.
  It was not part of these historical timed CLI runs.
- In the desktop, Split keeps the source header on the first result and creates
  blank editable headers for additional results. The benchmark commands use
  explicit deterministic fixture headers so exact historical output hashes
  remain reproducible. Headerless output stays headerless. An initial raw UTF-8
  BOM remains a file prefix, while a quoted first field beginning with the same
  bytes remains semantic data.
- Each transformed record is parsed and serialized with the resolved source
  delimiter and original CRLF, LF, or absent final line ending. Only one
  bounded record and its transformed fields are retained at a time.
- Save As writes beside the destination, flushes and syncs before no-clobber
  publication, and removes temporary output after cancellation or failure.
- The `transform-save-as` benchmark accepts one mutually exclusive `--split`
  or `--join` specification so each engine path is measured independently.
  Desktop repetition materializes the current working CSV, reopens it, and
  streams the next operation from that new working document.

## Environment

| Item | Value |
|---|---|
| Machine | MacBook Pro, Apple M3 Max, 16 CPU cores, 128 GB memory |
| OS | macOS 26.6.1, build 25G76 |
| Architecture | arm64 |
| Build | Cargo release profile, thin LTO |
| Storage | Source and destination on the local Data volume |
| Cache state | Warm after deterministic generation and source hashing |

The Save As timer and RSS snapshot stop before validation indexing. The
separate validation duration includes complete source and destination
structural indexes plus bounded first/middle/final reads. Current RSS was
unavailable from `ps` in the restricted benchmark process; peak RSS came from
`getrusage`, as in the existing Quarry benchmarks.

## Datasets

| Dataset | Generation | Bytes | Data rows | Source SHA-256 |
|---|---|---:|---:|---|
| Deterministic 1 GB | Seed 1, 11 columns | 1,000,000,077 | 5,117,757 | `afb0373394c884797ed77ec4a0fb915da0e0048c1161e205367a042745dea9c2` |
| Deterministic 12 GB | Seed 1, 11 columns | 12,000,000,037 | 61,413,211 | `cf1f0783dcc4bf5312378d1ae17e4361b6daac6a967e7a3b43f14d970411f84e` |

## Reproduction

Build the release benchmark and generate both deterministic fixtures:

```bash
cargo build --release -p quarry-cli
mkdir -p fixtures/generated
target/release/quarry-bench generate \
  --size 1GB --columns 11 --delimiter , \
  --output fixtures/generated/search-1gb.csv --seed 1
target/release/quarry-bench generate \
  --size 12GB --columns 11 --delimiter , \
  --output fixtures/generated/search-12gb.csv --seed 1
```

Run Split independently on either fixture:

```bash
target/release/quarry-bench transform-save-as SOURCE \
  --output split.csv --split 1 , 2 \
  --output-header column_1_prefix \
  --output-header column_1_suffix \
  --cache-state warm
```

Run Join independently on either fixture:

```bash
target/release/quarry-bench transform-save-as SOURCE \
  --output joined.csv --join 3,4 '|' \
  --output-header column_3_4 \
  --cache-state warm
```

Use `--cancel-after-bytes 67108864` on the Split command with an absent
destination to reproduce the cancellation runs. Use `shasum -a 256 SOURCE`
before and after each run to verify that Save As did not change the source.

## Complete Save As results

| Dataset | Transform | Output bytes | Save As time | Scan throughput | Output throughput | Validation indexes and reads | Peak RSS |
|---|---|---:|---:|---:|---:|---:|---:|
| Deterministic 1 GB | Split column 1 on `,` into 2 | 1,005,065,096 | 5.069 s | 188.15 MiB/s | 189.11 MiB/s | 1.970 s | 4.23 MiB |
| Deterministic 1 GB | Join columns 3,4 with `\|` | 1,000,000,070 | 5.226 s | 182.47 MiB/s | 182.47 MiB/s | 1.879 s | 4.17 MiB |
| Deterministic 12 GB | Split column 1 on `,` into 2 | 12,060,780,145 | 65.314 s | 175.22 MiB/s | 176.10 MiB/s | 23.946 s | 4.17 MiB |
| Deterministic 12 GB | Join columns 3,4 with `\|` | 12,000,000,030 | 63.289 s | 180.82 MiB/s | 180.82 MiB/s | 22.957 s | 4.17 MiB |

All four workers scanned the exact source byte total. Both transformations
validated the resulting header and schema, equal source/destination
record counts, and exact transformed values at the first, middle, and final
data rows.

| Dataset | Transform | Published output SHA-256 |
|---|---|---|
| Deterministic 1 GB | Split | `a046f1e77d882ab757b74d474ab3c709cc4d9c3c798dd81086b76a8604495872` |
| Deterministic 1 GB | Join | `9e1c66bf31f2b5506113a9211681619daa02cd819b5481d99bfb9875a48b28e5` |
| Deterministic 12 GB | Split | `9fa09f186f81341a24c9a3f15b486f5ce7f3e9aae3f9247990117bbd73ce2733` |
| Deterministic 12 GB | Join | `bd14c2bfd6605203aea83300ab0ab472631452ef888695f9f8815ecf66984f59` |

## Cancellation results

| Dataset | Threshold | Bytes scanned | Temporary bytes written | Worker time | Poll-inclusive latency | Peak RSS | Destination or temp left behind |
|---|---:|---:|---:|---:|---:|---:|---|
| Deterministic 1 GB | 64 MiB | 65 MiB | 64.32 MiB | 0.360 s | 2.694 ms | 4.22 MiB | No |
| Deterministic 12 GB | 64 MiB | 65 MiB | 64.32 MiB | 0.368 s | 3.020 ms | 4.20 MiB | No |

The source SHA-256 remained identical after both cancellation runs. The
benchmark also rejects a cancellation run if the worker reaches the end of the
file or publishes a destination before cancellation takes effect.

## Deterministic regression coverage

Core and CLI regressions cover:

- exact Split and Join output bytes with a multi-byte separator, padding,
  caller-selected Join order, CSV quoting, and unchanged source bytes;
- decoded delimiters, escaped quotes, embedded newlines, CRLF/LF/no final line
  ending, headered and headerless files, and ragged missing fields;
- an initial raw UTF-8 BOM followed by a quoted multiline first record with
  one-byte scanner chunks, plus headered first-column Split preservation;
- a headerless reversed-order Join whose first output field is quoted and
  multiline, plus a quoted first field beginning with BOM bytes as semantic
  data rather than a raw file prefix;
- a headerless, no-final-newline Join of two empty fields that serializes as
  `""` so reopening retains one empty record rather than a zero-byte file;
- Arrange reorder and deletion, missing-field padding, undiscovered trailing
  ragged-field preservation, unchanged source bytes, temporary-file cleanup,
  and sparse header/cell overlays applying before transformation;
- invalid, duplicate, or oversized column specifications and serialized-record
  limits; and
- cancellation with neither destination nor temporary output left behind.

## Acceptance

- [x] Split and Join each stream without a file-sized output model.
- [x] Core regressions confirm that header and cell overlays apply before each
  isolated persistence-engine transformation.
- [x] Header, resulting schema, record count, and first/middle/final data rows pass
  exact semantic validation on both deterministic datasets.
- [x] Save As leaves each source unchanged and publishes only complete output.
- [x] Cancellation leaves neither a destination nor temporary output.
- [x] Measured 1 GB and 12 GB peak RSS remain below the 500 MiB product target.
- [x] Measured 12 GB peak RSS stays within 32 MiB of the same transformation on
  1 GB.

## Limits of this evidence

All complete runs were warm-cache measurements, so this is not cold-cache
evidence. Semantic validation checks the header plus first, middle, and final
records after confirming complete record counts; it does not compare every
decoded output row. Deterministic whole-file hashes and exact small-fixture
regressions supplement those samples.

The memory comparison is scoped to one Split or one Join command with small
separator and header values. Record memory grows with schema width and field
sizes, though not with source-file size. The desktop keeps its ordinary bounded
viewport after materialization, displays at most 32 working-document columns at
a time, and uses the same 65,536-column structural trust limit as the core and
CLI. Desktop repetition is outside these one-operation measurements. The
private working CSV uses disk space proportional to the current document, plus
one adjacent working copy when retained for one-level structural Undo or Redo.
Move and Delete reuse the measured worker and have exact core regression
coverage, but no separate 1 GB or 12 GB timing is claimed for Arrange.
Replace All later reused the same bounded private rewrite worker, progress,
cancellation, guarded publication, and working-copy lifecycle. Exact core and
desktop regressions cover overlay-first replacement, non-overlapping matches,
no-match behavior, record limits, cancellation, cleanup, accessibility, and
Undo. This historical report does not claim separate 1 GB or 12 GB Replace All
timings.
