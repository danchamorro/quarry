# Streaming filtered-export validation: 2026-08-16

## Status

Implementation, deterministic regressions, and the 1 GB and 12 GB release
measurements are complete. Every acceptance gate passed.

## Decision

Measure filtered export through a dedicated CLI command in addition to the
desktop workflow. A CLI run gives reproducible worker progress, cancellation,
output, and process-memory evidence without including file-picker or render
timing. The command uses the same core export path as the UI.

## Contract under test

- Scan and parse the source in fixed-size chunks without retaining all matches.
- Apply the same literal, case-sensitive, decoded-cell AND predicates as the
  filter view.
- Copy the source header and matching raw records byte for byte, preserving the
  delimiter, quoting, line endings, and embedded newlines.
- Write beside the destination under a temporary name, sync completed output,
  and publish without overwriting an existing file.
- On cancellation or failure, publish no destination and remove the temporary
  file.
- Never modify the source or accept it as the destination.

## Environment

- Apple M3 Max MacBook Pro, 16 CPU cores, 128 GB RAM
- macOS 26.6.1 (25G76), arm64
- Rust 1.88.0
- working-tree release build based on `main` at `eec3514`, with the workspace
  lockfile
- fresh process for each recorded export
- cache state declared `unknown`

## Datasets

These identities were rechecked before recording export results.

| Dataset | Exact bytes | Physical records including header | Expected source SHA-256 |
|---|---:|---:|---|
| Deterministic 1 GB, seed 1, 11 columns | 1,000,000,077 | 5,117,758 | `afb0373394c884797ed77ec4a0fb915da0e0048c1161e205367a042745dea9c2` |
| `LARGE_FILE.csv` reference | 12,167,847,982 | 117,168,830 | `16b0469882c0ebf57f1b856144134ea60cc543f5394b246a97b5df721a4371f9` |

The deterministic positive query combines column 1 contains `with "quotes"`
and column 2 equals the decoded two-line value `line one\nline two`. The
generator schedule establishes exactly 251 matching data records. Because every
match exercises quoting, an embedded delimiter, doubled quotes, and an embedded
newline, its output also provides the primary byte-preservation check.

The complete 12 GB no-match query owns full-scan and cross-size RSS evidence. A
selective positive 12 GB run uses the first sampled `BRIDGE_ID` as an equality
predicate. The measurement found two identical matching records and produced a
302-byte output, so proving positive export did not require a multi-gigabyte
destination.

## Reproduction

```bash
cargo build --workspace --release --locked

RUN_DIR=$(mktemp -d /private/tmp/quarry-filtered-export.XXXXXX)
mkdir -p fixtures/generated "$RUN_DIR/cancel"

target/release/quarry generate --size 1GB --columns 11 --delimiter , \
  --output fixtures/generated/search-1gb.csv --seed 1

shasum -a 256 fixtures/generated/search-1gb.csv LARGE_FILE.csv

target/release/quarry-bench export fixtures/generated/search-1gb.csv \
  --output "$RUN_DIR/generated-1gb-filtered.csv" \
  --column 1 --operator contains --value 'with "quotes"' \
  --and 2 equals $'line one\nline two' --cache-state unknown

wc -c "$RUN_DIR/generated-1gb-filtered.csv"
shasum -a 256 "$RUN_DIR/generated-1gb-filtered.csv"

target/release/quarry-bench export LARGE_FILE.csv \
  --output "$RUN_DIR/reference-12gb-no-match.csv" \
  --column 1 --operator contains --value QUARRY_NO_MATCH_9F7B2C \
  --cache-state unknown

wc -c "$RUN_DIR/reference-12gb-no-match.csv"
shasum -a 256 "$RUN_DIR/reference-12gb-no-match.csv"

target/release/quarry-bench export LARGE_FILE.csv \
  --output "$RUN_DIR/reference-12gb-first-id.csv" \
  --column 1 --operator equals \
  --value ff3af50c-328c-414c-ada7-113e57d9cbc5 \
  --cache-state unknown

wc -c "$RUN_DIR/reference-12gb-first-id.csv"
shasum -a 256 "$RUN_DIR/reference-12gb-first-id.csv"

target/release/quarry open "$RUN_DIR/generated-1gb-filtered.csv" \
  --rows 5 --cache-state unknown
target/release/quarry open "$RUN_DIR/reference-12gb-no-match.csv" \
  --rows 5 --cache-state unknown
target/release/quarry open "$RUN_DIR/reference-12gb-first-id.csv" \
  --rows 5 --cache-state unknown

target/release/quarry-bench export LARGE_FILE.csv \
  --output "$RUN_DIR/cancel/should-not-exist.csv" \
  --column 1 --operator contains --value 0 \
  --cancel-after-bytes 67108864 --cache-state unknown

test ! -e "$RUN_DIR/cancel/should-not-exist.csv"
test -z "$(find "$RUN_DIR/cancel" -mindepth 1 -maxdepth 1 -print -quit)"

shasum -a 256 fixtures/generated/search-1gb.csv LARGE_FILE.csv
```

Run every recorded export in a fresh process. Do not label the cache cold or
warm unless it was controlled; use `unknown` otherwise. Record the exact
command output in the results tables so rows, bytes, elapsed time, cancellation
latency, and RSS remain auditable.

## Results

### Completed exports

| Dataset and predicate | Source bytes scanned | Records scanned | Matching rows written | Output bytes | Export time | Scan throughput | Output SHA-256 | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|---|---:|
| Deterministic 1 GB, two predicates | 1,000,000,077 | 5,117,758 | 251 | 58,486 | 1.883 s | 506.45 MiB/s | `891e706c530e4ab330b5f14583057a927573ef43815a455a7378879bbee2ed39` | 4.02 MiB |
| 12 GB, absent literal | 12,167,847,982 | 117,168,830 | 0 | 96 | 45.692 s | 253.96 MiB/s | `4efdd4019aa511b4ffefebb77207e4d5c18c2ae91d0fcb06c3ecb92f5bc04253` | 5.02 MiB |
| 12 GB, first sampled ID equals | 12,167,847,982 | 117,168,830 | 2 | 302 | 45.511 s | 254.98 MiB/s | `2f20f6e3f59c6d4faca4fda3b9af565a19348157d952db67a7a8d562d83b7a73` | 3.98 MiB |

The 1 GB worker reached the exact source byte and physical-record totals. It
reported 58,486 output bytes, and the published file had that exact size.
Reopening the output detected the original comma delimiter and header, then
indexed exactly 252 records: one header and 251 matching data records. The
first decoded rows retained the generated embedded delimiter, doubled quotes,
and two-line value. The automated byte comparison owns exact line-ending and
raw-record preservation.

Both 12 GB exports reached the exact source byte and physical-record totals.
The absent predicate published only the original 96-byte header. Reopening the
selective output found the header followed by two byte-identical GRACE HURT
records with the requested `BRIDGE_ID`.

### Cancellation

| Dataset | Requested | Final source bytes | Rows written before cancellation | Output bytes before cleanup | Cancellation latency | Destination absent | Temp absent | Peak RSS |
|---|---:|---:|---:|---:|---:|---|---|---:|
| 12 GB reference | 64 MiB | 68,157,440 | 547,857 | 55,902,270 | 3.781 ms | yes | yes | 5.02 MiB |

The cancellation figures report work completed before cleanup. The worker
stopped at 65 MiB, far before EOF, then removed the unpublished 53.31 MiB
temporary output. The destination directory was empty after the command.

### Source safety

| Dataset | SHA-256 before | SHA-256 after | Exact size before | Exact size after |
|---|---|---|---:|---:|
| Deterministic 1 GB | `afb0373394c884797ed77ec4a0fb915da0e0048c1161e205367a042745dea9c2` | `afb0373394c884797ed77ec4a0fb915da0e0048c1161e205367a042745dea9c2` | 1,000,000,077 | 1,000,000,077 |
| 12 GB reference | `16b0469882c0ebf57f1b856144134ea60cc543f5394b246a97b5df721a4371f9` | `16b0469882c0ebf57f1b856144134ea60cc543f5394b246a97b5df721a4371f9` | 12,167,847,982 | 12,167,847,982 |

## Automated coverage

- Core tests own byte-for-byte headers and records, decoded quoted and multiline
  predicates, progress totals, existing-destination rejection, source-path
  rejection, cancellation, failure, and temporary-file cleanup.
- CLI tests own the public argument path, exact output bytes, source preservation,
  successful cancellation reporting, and absence of both destination and temp.
- Timing and RSS thresholds belong to release measurements, not CI.

## Acceptance gates

- the deterministic 1 GB export reports exactly 251 matching data records and
  has a recorded exact byte count and SHA-256;
- its output preserves the expected header, quoted delimiter, doubled quotes,
  line endings, and multiline record boundaries;
- complete 1 GB and 12 GB runs reach their exact source byte and record totals;
- source size and SHA-256 are unchanged after all runs;
- the cancellation run stops before EOF, reports cancellation in under 100 ms,
  and leaves neither destination nor temporary file;
- deterministic 1 GB and 12 GB peak RSS remain below the 500 MiB product target;
- the 12 GB no-match peak RSS does not exceed the 1 GB peak RSS by more than
  32 MiB; and
- a successful destination is published only after complete output is flushed
  and synced.

The measured 12 GB no-match peak RSS was 1.00 MiB above the 1 GB export and
remained 494.98 MiB below the product ceiling. Phase 4 meets its exit gate.
