# Phase 6A stable text sort validation: 2026-08-21

## Status

Phase 6A is complete. The implementation, deterministic regressions, and
release measurements on the deterministic 1 GB and 12 GB fixtures all pass.

This report preserves the original Phase 6A release evidence. The current
engine uses adaptive merge fan-in and a 16 MiB initial-run budget; see the
[later 12 GB optimization benchmark](2026-08-23-12gb-sort-performance.md).

The 1 GB sort completed in 16.382 seconds with 16.88 MiB peak process RSS and
2.12 GiB measured peak temporary disk. The 12 GB sort completed in 253.132
seconds with 17.55 MiB peak process RSS and 24.65 GiB measured peak temporary
disk. Both outputs preserved the exact data-row count and raw header, matched
the source byte size, passed a complete sorted-order scan, verified bounded
record-multiset preservation and exact stable tie order before publication, and
left the source SHA-256 unchanged.

## Implemented behavior

- Sort every data row by exactly one selected numbered column.
- Use stable, bytewise, case-sensitive decoded text order, ascending or
  descending.
- Keep the header fixed, treat a missing ragged key as empty, and retain current
  row order for equal keys.
- Apply active header and cell edits before key comparison and output.
- Reopen a successful result as the ordinary Modified grid with Save, Save As,
  Discard, Undo, and Redo.
- Preserve the source and current document on cancellation, failure, or a
  source conflict.

## Original resource and publication bounds

The original Phase 6A worker built 8 MiB runs and spilled owner-only framed
files. Merge heap entries retained keys and record lengths, while only one
pending key and one selected record body were loaded at a time. Its effective
merge fan-in reserved two maximum-record payloads before admitting heap keys;
the default 64 MiB record cap therefore used fan-in two and stayed within the
256 MiB payload budget. Multipass run files and guarded output used owner-only
storage and were removed after success, cancellation, or failure.

Before sorting, the desktop waits for indexing to finish and reports a
conservative temporary-disk allowance. The bound is four times the effective
file-size upper bound plus 48 bytes per data row. The effective-size bound
includes a two-byte-per-row fidelity cushion plus committed and active sparse
values. This covers duplicated sort keys, 24-byte run frames, two temporarily
coexisting run generations, and guarded output.

Cancellation is checked while scanning, seeding and merging runs, and before
the final flush and sync. Guarded publication checks the source again before
exposing the complete working CSV. Scan progress changes to an active merge
phase rather than showing 100 percent while merge work remains.

The validation CLI reports the worker's wall time, peak temporary bytes, merge
passes, exact data/header counts, cancellation latency, peak process RSS, and
streaming FNV-1a fingerprints. Completion also requires a bounded dual
fingerprint of the effective record multiset and exact increasing source
ordinals for adjacent equal keys. Raw headers are compared in fixed 64 KiB
chunks. SHA-256 is recorded separately before and after the runs using the
platform `shasum` tool.

## Regression evidence

`cargo test --workspace --locked --offline` passed 195 tests:

| Package | Tests |
|---|---:|
| AppKit | 1 |
| CLI | 21 |
| Core | 101 |
| Delimited parser | 9 |
| egui desktop | 63 |

The sort-specific regressions cover stable ascending and descending order,
quoted multiline keys, ragged rows, sparse overlays, header renames, BOM and
unterminated-record handling, forced multipass merging, the merge-memory bound,
the disk formula, source conflicts, output record limits, owner-only files,
cancellation cleanup, record-multiset preservation, exact equal-key ordinal
order, cancellation during initial-run flushing, accessibility, visible merge
progress, and structural Undo and Redo. Focused regressions also verify the
measured peak-temporary-disk, merge-pass, header-count, streamed raw-header
comparison, frozen-elapsed-time, cancellation-latency, observed permissions,
and file fingerprint reporting paths.

Strict workspace linting, formatting, and whitespace checks also pass.

## Environment

| Item | Value |
|---|---|
| Machine | MacBook Pro, Apple M3 Max, 16 cores |
| Memory | 128 GB |
| Operating system | macOS 26.6.1 (25G76) |
| Rust compiler | rustc 1.88.0 |
| Build | `cargo build --release --locked --offline -p quarry-cli --bin quarry-bench` |
| Cache state | Warm after deterministic generation and source hashing |

The restricted validation session did not expose current RSS through `ps` and
blocked one `/usr/bin/time -l` system query. Peak sort RSS came from the same
`getrusage` path used by the existing Quarry benchmarks. Worker wall time is
measured internally and excludes the later validation indexes, order scan, and
file fingerprints.

## Datasets

| Dataset | Generation | Bytes | Data rows | Source SHA-256 | Source FNV-1a 64 |
|---|---|---:|---:|---|---|
| Deterministic 1 GB | Seed 1, 11 columns | 1,000,000,077 | 5,117,757 | `afb0373394c884797ed77ec4a0fb915da0e0048c1161e205367a042745dea9c2` | `7a35e81842428486` |
| Deterministic 12 GB | Seed 1, 11 columns | 12,000,000,037 | 61,413,211 | `cf1f0783dcc4bf5312378d1ae17e4361b6daac6a967e7a3b43f14d970411f84e` | `f70393c775be818d` |

The deterministic 12 GB fixture is `fixtures/generated/search-12gb.csv`. It is
not the separate 12,167,847,982-byte `LARGE_FILE.csv` viewer reference.

## Reproduction

Build the release validation binary and generate the fixtures:

```bash
cargo build --release --locked --offline -p quarry-cli --bin quarry-bench
target/release/quarry-bench generate \
  --size 1GB --columns 11 --delimiter , \
  --output fixtures/generated/search-1gb.csv --seed 1
target/release/quarry-bench generate \
  --size 12GB --columns 11 --delimiter , \
  --output fixtures/generated/search-12gb.csv --seed 1
```

Run each successful validation with an absent owner-only destination:

```bash
target/release/quarry-bench sort-save-as \
  fixtures/generated/search-1gb.csv sorted-1gb.csv \
  --column 1 --order asc --header first-row --cache-state warm
target/release/quarry-bench sort-save-as \
  fixtures/generated/search-12gb.csv sorted-12gb.csv \
  --column 1 --order asc --header first-row --cache-state warm
```

The destination is owner-only and must not already exist. Use `--order desc`
for descending validation. Add `--cancel-after-bytes 67108864` and use an
absent destination for the cancellation runs. No cancellation destination or
temporary artifact should remain.

## Successful sort results

| Dataset | Sort time | Throughput | Peak RSS | Estimated temporary disk | Measured peak temporary disk | Runs | Merge passes | Validation scan |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Deterministic 1 GB | 16.382 s | 58.21 MiB/s | 16.88 MiB | 4,286,594,700 bytes (3.99 GiB) | 2,279,612,666 bytes (2.12 GiB) | 289 | 8 | 8.515 s |
| Deterministic 12 GB | 253.132 s | 45.21 MiB/s | 17.55 MiB | 51,439,139,964 bytes (47.91 GiB) | 26,470,708,694 bytes (24.65 GiB) | 3,453 | 11 | 100.726 s |

Both outputs had exactly one 101-byte raw header and the exact source data-row
count. Output byte size equaled source byte size, every row passed the complete
ascending-order scan, the bounded record-multiset and exact stable-tie checks
passed before publication, and owner-only permissions were retained.

| Dataset | Output bytes | Output SHA-256 | Output FNV-1a 64 |
|---|---:|---|---|
| Deterministic 1 GB | 1,000,000,077 | `c1389d7e383a6a6344420d2c068be91e28fcb33329f4b90bf4bda7e9363a00a9` | `c99cc19ac13b4fcc` |
| Deterministic 12 GB | 12,000,000,037 | `3925f504abd37d3f65c6e1069223bd4565afb97f783f2382ebbfd434dbd7aadd` | `e5524a78798152b9` |

## Cancellation results

| Dataset | Requested threshold | Bytes scanned | Rows processed | Temporary bytes | Worker time | Cancellation latency | Peak RSS | Published or leftover artifact |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| Deterministic 1 GB | 64 MiB | 68,157,440 | 345,177 | 75,496,242 | 0.284 s | 3.052 ms | 16.64 MiB | No |
| Deterministic 12 GB | 64 MiB | 68,157,440 | 343,442 | 75,496,242 | 0.277 s | 3.035 ms | 16.81 MiB | No |

Both cancellation runs stopped before the full source scan, published no
destination, removed all run and staging files, and retained the original
source SHA-256 values.

## Gate result

- [x] Stable ascending text order passes a complete output scan on 1 GB and
  12 GB.
- [x] Exact data-row count, raw header, and output byte-size preservation pass.
- [x] Bounded effective-record multiset evidence and exact equal-key source
  ordinal checks pass before publication.
- [x] Peak RSS stays bounded and the 12 GB result remains within 32 MiB of the
  1 GB result.
- [x] Measured peak temporary disk remains below the conservative preflight
  estimate on both fixtures.
- [x] Source SHA-256 remains unchanged after success and cancellation.
- [x] Cancellation completes within 10 ms and leaves no destination, sort run,
  or staging file.

Phase 6A exits successfully.
