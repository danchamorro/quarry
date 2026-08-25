# Multiple AND-predicate filter validation: 2026-08-16

> Current behavior note (2026-08-24): Quarry now treats Equals and Contains
> values within one column as alternatives, applies all Does not equal rules,
> and requires every filtered column to match. The results below preserve the
> original distinct-column AND benchmark evidence.

## Decision

Ship multiple literal filter predicates under the current grouped contract.
Matching is ASCII case-insensitive by default; **Match case** selects exact byte
matching. Within one source column, Equals and Contains values are alternatives,
while every Does not equal predicate excludes its value. Every filtered source
column must match (AND). Quarry parses each record once and retains only adaptive
match checkpoints under the existing fixed memory budget.

The deterministic 1 GB intersection returned the exact 251 matches with a
5.88 KiB filter index and 5.16 MiB peak process RSS. The 12 GB scan reached the
exact end of the file with 3.92 MiB peak RSS. At the time of this measurement,
streaming filtered export was the next Phase 4 slice. It is now complete and
documented in the [filtered-export validation](2026-08-16-filtered-export.md).
These recorded 2026-08-16 measurements used two distinct filtered columns under
the then-current case-sensitive contract. They remain historical evidence for
exact distinct-column intersection, end-of-file scanning, and bounded memory.
Arbitrary Boolean grouping, regex, and locale-aware Unicode case folding remain
deferred.

## Implementation

- A `FilterPredicate` owns one source column, a Contains, Equals, or Does not
  equal operator, and a literal byte value. A nonempty `FilterQuery` owns the
  ordered predicate list and its comparison mode.
- The scanner parses each bounded record once and groups predicates by source
  column. A column passes when it matches at least one Equals or Contains value,
  if present, and matches none of its Does not equal values. All filtered
  columns must pass. A missing filtered column rejects the record.
- The default comparison folds ASCII letters only. **Match case** compares exact
  bytes. Other bytes are unchanged in either mode.
- The adaptive filter index owns the complete query, exact match count, and
  bounded checkpoints. Filtered row reads reevaluate the same query from the
  nearest checkpoint instead of retaining every matching row.
- The CLI keeps `--column`, `--operator`, and `--value` as the first predicate.
  Repeatable `--and COLUMN contains|equals|not-equals VALUE` triples add rules;
  their columns determine the grouped matching behavior.
- The egui Filters window supports adding and removing rules plus its own
  **Match case** option. Cell context-menu filters inherit that setting.
  Filtering and match-only navigation stay in background workers.

## Environment

- Apple M3 Max MacBook Pro, 16 CPU cores, 128 GB RAM
- macOS 26.6.1 (25G76), arm64
- Rust 1.88.0
- working-tree release build based on `main` at `f1826b8`, with the workspace
  lockfile
- fresh process for each recorded command
- cache state declared `unknown`

## Datasets

| Dataset | Exact bytes | Physical records including header | SHA-256 |
|---|---:|---:|---|
| Deterministic 1 GB, seed 1, 11 columns | 1,000,000,077 | 5,117,758 | `afb0373394c884797ed77ec4a0fb915da0e0048c1161e205367a042745dea9c2` |
| `LARGE_FILE.csv` reference | 12,167,847,982 | 117,168,830 | `16b0469882c0ebf57f1b856144134ea60cc543f5394b246a97b5df721a4371f9` |

The deterministic generator places decoded `with "quotes"` values in column 1
and decoded `line one\nline two` values in column 2 on known schedules. Their
AND intersection has exactly 251 matches.

`LARGE_FILE.csv` has a different data profile. Zero matches are expected for
this exact query only because both generator-specific literals are absent. The
12 GB result is therefore evidence of exact end-of-file scanning and bounded
memory, while the deterministic 1 GB result owns the exact-intersection proof.

## Reproduction

Generate the deterministic 1 GB fixture with the command in the
[streaming search benchmark](2026-08-15-streaming-search.md#reproduction), then
run the commands below to reproduce the 251-match result.

```bash
cargo build --workspace --release --locked

target/release/quarry-bench filter fixtures/generated/search-1gb.csv \
  --column 1 --operator contains --value 'with "quotes"' \
  --and 2 equals $'line one\nline two' --cache-state unknown

target/release/quarry-bench filter LARGE_FILE.csv \
  --column 1 --operator contains --value 'with "quotes"' \
  --and 2 equals $'line one\nline two' --cache-state unknown

target/release/quarry-bench filter LARGE_FILE.csv \
  --column 1 --operator contains --value 'with "quotes"' \
  --and 2 equals $'line one\nline two' \
  --cancel-after-bytes 67108864 --cache-state unknown
```

## Recorded results

### Complete scans

| Dataset | Matches | Exact bytes scanned | Physical records scanned | Filter time | Throughput | Filter-index memory | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|
| Deterministic 1 GB | 251 | 1,000,000,077 | 5,117,758 | 1.759 s | 542.09 MiB/s | 5.88 KiB | 5.16 MiB |
| 12 GB reference | 0 | 12,167,847,982 | 117,168,830 | 40.918 s | 283.59 MiB/s | 0 B | 3.92 MiB |

The first 1 GB match was ordinal 1 at data row 1 and byte offset 101. The last
sampled match was ordinal 251 at data row 5,116,751 and byte offset
999,803,301. The bounded first, middle, and final windows read 226 unique rows
in 1497.205 ms with checksum `12572588509387389060`.

The 12 GB scan reported no first or last match, as expected for these absent
literals. It still reached the exact byte and physical-record totals before
reporting zero.

### Cancellation

| Dataset | Requested | Final bytes | Physical records | Filter time | Throughput | Poll-inclusive cancellation latency | Matches | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 12 GB reference | 64 MiB | 68,157,440 (65 MiB) | 667,952 | 0.230 s | 282.98 MiB/s | 1.515 ms | 0 | 3.92 MiB |

The one-chunk overshoot is expected because progress publishes after each
1 MiB read. Cancellation stopped before end of file.

## Automated coverage

- The workspace passed 67 tests: 26 core, six CLI, 28 egui, six delimited
  parser, and one AppKit test.
- Core regressions cover AND intersection, validation, ragged rows, decoded
  quoted and multiline values, bounded reads, and cancellation.
- CLI regressions cover the original single-predicate form, repeatable `--and`
  triples, and invalid rules. The release benchmark above owns the exact CLI
  intersection result.
- egui regressions cover add/remove rule controls, AND application, focus and
  accessibility, match-only navigation, cancellation, clearing, and reopen.
- Formatting, strict Clippy across all targets and features, and the locked
  workspace release build passed.

## Viewer smoke test

Computer Use opened the deterministic 1 GB file in the release egui app, added
the same two rules through **Filters...**, and entered the equality value as a
literal two-line field. The panel exposed both numbered rules and reported that
all rules must match. Progress and the match-only grid updated while filtering
continued in the background.

The completed view reported exactly 251 filter matches. Its first visible rows
were source data rows 1, 20,468, and 40,935, matching the deterministic
intersection schedule. The active summary preserved both source columns,
operators, and the escaped multiline display value.

## Acceptance gates

- the deterministic 1 GB query returns exactly 251 AND matches;
- the 12 GB zero-match scan reaches exact end-of-file byte and record counts;
- the 12 GB cancellation run stops before end of file and remains below 100 ms;
- filter-index memory remains within its fixed budget;
- peak RSS remains below the 500 MiB product target; and
- single-predicate commands and reads remain compatible.

All listed gates passed.

## Limits

- Filter grouping supports alternative Equals and Contains values within one
  column, all Does not equal exclusions within that column, and AND across
  filtered columns. Arbitrary nested AND/OR expressions are not supported.
- Matching is literal and ASCII case-insensitive by default, with exact byte
  matching available through **Match case**. Regex, wildcard, locale-aware, and
  full Unicode case folding are not supported.
- Filter construction remains a sequential full scan.
- Filtered reads may rescan records between retained match checkpoints.
- The filter index remains process-local and is not persisted.
- The recorded results cover the historical case-sensitive, distinct-column
  query. They do not separately benchmark same-column alternatives, Does not
  equal predicates, or ASCII-insensitive matching.
- Streaming filtered export was not part of this validation; it is covered by
  the later [filtered-export validation](2026-08-16-filtered-export.md).
- Peak RSS comes from the process `getrusage` measurement used by the other
  Quarry benchmarks.
