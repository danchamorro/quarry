# Direct cell editing and Save As validation: 2026-08-18

## Summary

Quarry now keeps direct data-cell changes in the same sparse unsaved overlay as
header renames and streams that overlay through Save and Save As. A release
benchmark applied three edits to deterministic 1 GB and 12 GB files, published
new files, indexed each published file, and read the edited rows back to verify
the exact requested cell values.

The 1 GB Save As completed in 1.524 seconds at 625.78 MiB/s with 5.05 MiB peak
process RSS. The 12 GB Save As completed in 16.581 seconds at 690.18 MiB/s with
4.00 MiB peak process RSS. The 12 GB input was 12 times larger while the
measured peak RSS was 1.05 MiB lower. This is evidence for these two datasets
and the same three-edit workload, not a claim that arbitrary edit sets have
constant memory. Overlay memory grows with the number and size of edits.

Cancellation after the 64 MiB threshold stopped both runs at 65 MiB scanned,
published no destination, removed temporary output, and left each source hash
unchanged.

## Implementation under test

- The viewer keys committed cell edits by zero-based physical record row and
  original source column. Display order and hidden columns do not change that
  identity.
- Save and Save As scan with the quote-aware record scanner. Unedited records
  are copied byte for byte. An edited record is parsed once and serialized with
  the document delimiter, quoting, and its original CRLF, LF, or absent final
  line ending.
- The worker retains a fixed read chunk, at most one record and its decoded
  fields, and the sparse edit overlay. It enforces the 64 MiB serialized-record
  limit before publication.
- The `edit-save-as` benchmark command accepts one or more one-based
  `--edit DATA_ROW COLUMN VALUE` triples. A successful run indexes the
  destination and reads every requested cell back before reporting validation
  success.

## Environment

| Item | Value |
|---|---|
| Machine | MacBook Pro, Apple M3 Max, 16 CPU cores, 128 GB memory |
| OS | macOS 26.6.1, build 25G76 |
| Build | Cargo release profile, thin LTO |
| Storage | Source and destination on the local Data volume |
| 1 GB cache state | Unknown |
| 12 GB cache state | Warm after deterministic generation and source hashing |

The command's Save As timer and RSS snapshot stop before destination indexing
and row validation. The separate validation duration is reported explicitly.
Current RSS was unavailable from `ps` in the restricted benchmark process;
peak RSS came from `getrusage`, as in the existing Quarry benchmarks.

## Datasets

| Dataset | Generation | Bytes | Source SHA-256 |
|---|---|---:|---|
| Deterministic 1 GB | Generated below as `fixtures/generated/search-1gb.csv`, seed 1, 11 columns | 1,000,000,077 | `afb0373394c884797ed77ec4a0fb915da0e0048c1161e205367a042745dea9c2` |
| Deterministic 12 GB | Generated for this validation, seed 1, 11 columns | 12,000,000,037 | `cf1f0783dcc4bf5312378d1ae17e4361b6daac6a967e7a3b43f14d970411f84e` |

## Reproduction

Build the release benchmark:

```bash
cargo build --release -p quarry-cli
```

Generate both deterministic fixtures:

```bash
mkdir -p fixtures/generated
target/release/quarry-bench generate \
  --size 1GB --columns 11 --delimiter , \
  --output fixtures/generated/search-1gb.csv --seed 1
target/release/quarry-bench generate \
  --size 12GB --columns 11 --delimiter , \
  --output generated-12gb.csv --seed 1
```

Run the deterministic 1 GB sparse edit:

```bash
target/release/quarry-bench edit-save-as \
  fixtures/generated/search-1gb.csv \
  --output edited-1gb.csv \
  --edit 1 1 QUARRY_EDIT_FIRST_20260818 \
  --edit 2558879 6 QUARRY_EDIT_MIDDLE_20260818 \
  --edit 5117757 11 QUARRY_EDIT_LAST_20260818 \
  --cache-state unknown
```

Run the deterministic 12 GB sparse edit:

```bash
target/release/quarry-bench edit-save-as generated-12gb.csv \
  --output edited-12gb.csv \
  --edit 1 1 QUARRY_EDIT_FIRST_20260818 \
  --edit 30000000 6 QUARRY_EDIT_MIDDLE_20260818 \
  --edit 60000000 11 QUARRY_EDIT_DEEP_20260818 \
  --cache-state warm
```

Use `--cancel-after-bytes 67108864` with an absent destination to reproduce
the cancellation runs. Use `shasum -a 256 SOURCE` before and after each run to
verify that Save As did not change the source.

## Complete Save As results

| Dataset | Sparse edits | Bytes scanned | Output bytes | Save As time | Scan throughput | Validation time | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|
| Deterministic 1 GB | 3 | 1,000,000,077 | 1,000,000,090 | 1.524 s | 625.78 MiB/s | 0.978 s | 5.05 MiB |
| Deterministic 12 GB | 3 | 12,000,000,037 | 12,000,000,050 | 16.581 s | 690.18 MiB/s | 11.812 s | 4.00 MiB |

Both workers reached the exact source byte total. Both destinations were
published only after the worker completed. Destination indexing and bounded
row reads then found all three exact edited values.

| Dataset | Source SHA-256 before and after | Published output SHA-256 |
|---|---|---|
| Deterministic 1 GB | `afb0373394c884797ed77ec4a0fb915da0e0048c1161e205367a042745dea9c2` | `a151d15ef1133f77b4466402ab3382148efa50cd09660c07b05c9ba94c0e4a43` |
| Deterministic 12 GB | `cf1f0783dcc4bf5312378d1ae17e4361b6daac6a967e7a3b43f14d970411f84e` | `959cc4017e8987d2976dba6f0380b4986bfb68002a5a3bbf504ce121b9bf24e4` |

## Cancellation results

| Dataset | Threshold | Bytes scanned | Bytes written to temporary output | Worker time | Poll-inclusive latency | Peak RSS | Destination or temp left behind |
|---|---:|---:|---:|---:|---:|---:|---|
| Deterministic 1 GB | 64 MiB | 65 MiB | 64 MiB | 0.110 s | 2.534 ms | 3.97 MiB | No |
| Deterministic 12 GB | 64 MiB | 65 MiB | 64 MiB | 0.110 s | 2.538 ms | 3.97 MiB | No |

The source SHA-256 remained identical after each cancellation. The benchmark
also rejected a run if the worker reached the end of the file or published a
destination before cancellation took effect.

## Deterministic regression coverage

`cargo test -p quarry-cli` passed 12 tests. The direct-edit cases cover:

- exact source preservation and exact destination bytes;
- an edited field containing a delimiter, quotes, and an embedded newline;
- CRLF records, an unedited record copied byte for byte, and a final record
  without a line ending;
- multiple sparse cell edits and destination read-back validation;
- invalid one-based coordinates and duplicate edit rejection; and
- mid-scan cancellation with no destination or temporary output left behind.

Core regressions additionally cover headered and headerless row identity,
multiple edits in one record, ragged-row bounds, serialized-record limits,
permissions, source-change detection, symbolic-link policy, no-clobber Save As
publication, and Save conflict handling.

## Acceptance

- [x] Sparse edits use stable source row and column identities.
- [x] Quoted and multiline values serialize correctly.
- [x] Unedited records remain byte-preserving.
- [x] Save As leaves the source unchanged and publishes only a complete output.
- [x] Cancellation leaves neither a destination nor temporary output.
- [x] Exact requested values are read back from each published output.
- [x] The measured 1 GB and 12 GB peak RSS values remain below the 500 MiB
  product target.
- [x] The measured 12 GB peak RSS does not exceed the 1 GB peak RSS by more
  than 32 MiB for the same three-edit workload.

## Limits of this evidence

The 1 GB and 12 GB runs had different cache states, so their throughput is not
a controlled cold-cache comparison. The RSS comparison is scoped to three
sparse edits of small values. It does not measure thousands of edits or values
near the maximum record size. Save's metadata conflict checks reduce but do not
eliminate the final replacement race already described in the architecture.
