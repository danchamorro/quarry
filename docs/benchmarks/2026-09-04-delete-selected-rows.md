# Delete Selected Rows validation: 2026-09-04

## Summary

Quarry's Phase 8A row-deletion path completed direct release runs on 1 GB and
12 GB CSV files while retaining less than 4 MiB peak process RSS. Each run
deleted the first data record, preserved the header, scanned the complete
source, published a private working CSV, and left the source hash unchanged.

The 1 GB run completed in 1.497 seconds at 637.06 MiB/s. The 12 GB run
completed in 23.698 seconds at 489.67 MiB/s. Both resulting files reopened
successfully with the expected comma delimiter and header.

## Implementation under test

- Row selections are stored as compact zero-based physical-record ranges.
- The worker streams the current CSV once, skips selected records, applies
  sparse edits to retained records, and preserves unedited records byte for
  byte.
- Output remains a private working CSV until Save or Save As publishes it.
- Cancellation, failure, and source conflict leave the current document and
  source unchanged while removing unpublished output.
- Filtering clears row selection, and row selection plus deletion remain
  unavailable while a filter is active.

## Environment

| Item | Value |
|---|---|
| Machine | MacBook Pro, Apple M3 Max, 16 CPU cores, 128 GB memory |
| OS | macOS 26.6.2, build 25G83 |
| Architecture | arm64 |
| Build | Cargo release profile, thin LTO |
| Storage | Source and destination on the local Data volume |
| Cache state | Warm after generation or source hashing |

Peak RSS came from macOS `/usr/bin/time -l`. Worker elapsed time and byte
counts came from the public `SaveAsJob` progress and completion APIs.

## Datasets

| Dataset | Bytes | Columns | Source SHA-256 before and after |
|---|---:|---:|---|
| Deterministic 1 GB, seed 8 | 1,000,000,154 | 16 | `930de19bcf3f2f1ff1d03bc2c60de79972014d3c6cf941e8117ae196f277d132` |
| `LARGE_FILE_12GB.csv` | 12,167,847,982 | 11 | `16b0469882c0ebf57f1b856144134ea60cc543f5394b246a97b5df721a4371f9` |

The 1 GB fixture was generated with:

```bash
target/release/quarry generate \
  --size 1GB --columns 16 --delimiter , \
  --output /private/tmp/quarry-phase8a-1gb.csv --seed 8
```

The measured operation called
`Session::start_create_working_copy_deleting_rows` with empty header and cell
edit maps, physical record range `1..=1`, and a new destination. The one-off
example used to invoke that public API was removed after validation rather than
adding a product command for one benchmark.

## Results

| Dataset | Records deleted | Bytes scanned | Output bytes | Worker time | Throughput | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|
| Deterministic 1 GB | 1 | 1,000,000,154 | 999,995,782 | 1.497 s | 637.06 MiB/s | 3.86 MiB |
| `LARGE_FILE_12GB.csv` | 1 | 12,167,847,982 | 12,167,847,879 | 23.698 s | 489.67 MiB/s | 3.84 MiB |

The destination size decreased by exactly the selected record length: 4,372
bytes for the deterministic 1 GB file and 103 bytes for the 12 GB file. The
source SHA-256 values matched before and after each run. Both destinations then
passed a release `quarry open --metrics-only --no-wait` check with a detected
header and comma delimiter.

## Deterministic regression coverage

The locked workspace test suite covers:

- normal, range, and additive row selection plus accessible selection state;
- headered, headerless, quoted, multiline, invalid-row, UTF-8 BOM, and sparse
  edit behavior;
- deletion of a selected record larger than the read chunk without buffering
  that record;
- working-copy installation, Save, Save As, Discard Changes, Undo, and Redo;
- filtered-view blocking and selection clearing; and
- cancellation, source conflict, failure cleanup, and source preservation.

## Acceptance

- [x] Row deletion preserves the header and unselected records.
- [x] The source remains byte-identical until Save.
- [x] Completed output reopens through the normal viewer path.
- [x] Peak RSS remains below the 500 MiB product target on 1 GB and 12 GB.
- [x] The 12 GB peak RSS does not exceed the 1 GB result.
- [x] Exact regressions cover cancellation, cleanup, accessibility, and history.

## Limits of this evidence

These timings cover deletion of one early data record from warm local files.
They do not claim controlled cold-cache throughput. Selection metadata grows
with the number of disjoint selected ranges, and sparse edit memory grows with
the number and size of edits. Exact edge-case correctness comes from the
deterministic regression suite; the large-file runs validate the production
streaming path and bounded memory behavior.
