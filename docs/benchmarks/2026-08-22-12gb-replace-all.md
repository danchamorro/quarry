# 12 GB Replace All benchmark: 2026-08-22

## Summary

Quarry replaced 291,058 literal matches across a deterministic 12 GB CSV in
61.541 seconds. The production Replace All worker scanned and published at
185.96 MiB/s with 4.20 MiB peak process RSS. The source byte size remained
unchanged, and the complete destination was published at that exact size.

This run adds a direct 12 GB Replace All measurement to the existing 12 GB
feature suite. Exact small-fixture regressions remain responsible for decoded
field-by-field semantics and cancellation cleanup.

## Environment

| Item | Value |
|---|---|
| Machine | MacBook Pro, Apple M3 Max, 16 cores |
| Memory | 128 GB |
| Operating system | macOS 26.6.1 (25G76) |
| Build | Cargo release profile |
| Cache state | Warm after deterministic generation and source hashing |

Current RSS was unavailable from the restricted benchmark process. Peak RSS
came from the same `getrusage` path used by the other Quarry benchmarks.

## Fixture

| Property | Value |
|---|---:|
| Exact bytes | 12,000,000,037 |
| Binary size | 11.18 GiB |
| Data rows | 61,413,211 |
| Columns | 11 |
| Generator seed | 1 |
| Source SHA-256 | `cf1f0783dcc4bf5312378d1ae17e4361b6daac6a967e7a3b43f14d970411f84e` |

This is the deterministic write-heavy fixture used by Quarry's 12 GB editing,
transformation, and sorting validations. It is separate from the
12,167,847,982-byte viewer reference file.

## Reproduction

Build the release benchmark and generate the deterministic fixture:

```bash
cargo build --release --locked -p quarry-cli --bin quarry-bench
target/release/quarry-bench generate \
  --size 12GB --columns 11 --delimiter , \
  --output generated-12gb.csv --seed 1
```

Run Replace All with an absent destination:

```bash
target/release/quarry-bench replace-all-save-as generated-12gb.csv \
  --output replaced-12gb.csv \
  --query 'line one' --replacement 'line uno' \
  --cache-state warm
```

The query and replacement have the same byte length, so the expected output
size is exactly the source size.

## Results

| Measurement | Result |
|---|---:|
| Replacements | 291,058 |
| Bytes scanned | 12,000,000,037 |
| Bytes published | 12,000,000,037 |
| Worker time | 61.541 s |
| Scan throughput | 185.96 MiB/s |
| Output throughput | 185.96 MiB/s |
| Peak process RSS | 4.20 MiB |
| Output SHA-256 | `7c7f48bbb975eeba03699fd683df0f4f49efa25105911381aa0ae40a415a91be` |

The benchmark required the worker's replacement count and progress totals to
match the published destination. It also checked that the source byte size was
unchanged and that the destination existed only after successful completion.
The recorded source hash matches the deterministic fixture used by the prior
12 GB validation gates.

## Limits

- The run followed fixture generation and hashing, so it is labeled warm.
- No second decoded full-file comparison was performed after publication.
- Exact focused CLI regressions verify replacement semantics, source
  preservation, no-match behavior, cancellation, and temporary-file cleanup.
- Current RSS was unavailable; only peak process RSS is reported.

The generated source, destination, and temporary benchmark directory were
removed after recording and validating the results.
