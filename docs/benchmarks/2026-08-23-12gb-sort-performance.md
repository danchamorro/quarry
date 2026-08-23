# 12 GB `FIRSTNAME` sort optimization: 2026-08-23

## Result

Quarry sorted all 117,168,829 data rows in the 11.33 GiB viewer fixture by
column 2, `FIRSTNAME`, in 142.211 seconds. The optimized sort was 2.36 times
faster than the 335.837-second baseline and reduced worker time by 57.7%.

The final output was byte-for-byte identical to the baseline output. Complete
validation also passed exact row and raw-header preservation, ascending order,
stable equal-key order, a bounded dual record-multiset fingerprint, output
fingerprinting, and source-size preservation.

## What changed

The external sort now chooses merge fan-in from the largest key actually seen
in the source instead of assuming every retained key could equal the 64 MiB
maximum record size. Short first names can therefore use the configured
32-way merge while pathological wide keys still fall back to the bounded safe
fan-in.

The default initial-run budget increased from 8 MiB to 16 MiB. On this file,
that is the smallest tested increase that brought the total merge work down to
two passes. No dependency, new sort mode, or undo setting was added.

## Dataset and environment

| Item | Value |
|---|---|
| Source | `LARGE_FILE_12GB.csv` |
| Source bytes | 12,167,847,982 (11.33 GiB) |
| Data rows | 117,168,829 |
| Columns | 11, comma-delimited |
| Header | First row |
| Sort key | Column 2, `FIRSTNAME` |
| Order | Ascending, stable, case-sensitive text |
| Source SHA-256 | `16b0469882c0ebf57f1b856144134ea60cc543f5394b246a97b5df721a4371f9` |
| Source FNV-1a 64 | `213fb9426293686f` |
| Machine | MacBook Pro, Apple M3 Max, 16 cores, 128 GB memory |
| Operating system | macOS 26.6.1 (25G76) |
| Cache state | Warm |

Worker time excludes the later benchmark-only validation pass. Peak RSS comes
from the benchmark process `getrusage` measurement.

## Before and after

| Engine configuration | Sort time | Throughput | Total runs created | Merge passes | Peak RSS | Peak temporary disk |
|---|---:|---:|---:|---:|---:|---:|
| Baseline: 8 MiB runs, worst-case-key fan-in | 335.837 s | 34.55 MiB/s | 3,737 | 11 | 23.75 MiB | 25.91 GiB |
| Adaptive fan-in, 8 MiB runs | 156.497 s | 74.14 MiB/s | 1,928 | 3 | 23.36 MiB | 25.91 GiB |
| Adaptive fan-in, 16 MiB runs | **142.211 s** | **81.60 MiB/s** | **964** | **2** | **49.89 MiB** | **25.91 GiB** |

The final validation scan took 271.347 seconds. Its cost is not included in the
142.211-second application sort time.

## Correctness evidence

| Check | Result |
|---|---|
| Output bytes | 12,167,847,982 |
| Output FNV-1a 64 | `c373469a35afcabd` |
| Exact data row count | 117,168,829, passed |
| Exact raw header | 96 bytes, passed |
| Complete ascending-order scan | Passed |
| Stable equal-key source ordinals | Passed |
| Bounded record multiset | Passed, dual fingerprint |
| Source size unchanged | Passed |
| Baseline and optimized output comparison | Byte-for-byte identical |
| Published permissions | Owner-only, mode `0600` |

## Cancellation

Cancellation was requested after 64 MiB on the final 16 MiB configuration.
The worker stopped after scanning 65 MiB, with 3.168 ms cancellation latency
and 37.66 MiB peak RSS. It published no destination and left no sort artifact.

## Undo assessment

Disabling undo does not speed up a clean initial sort in Quarry. Undo retains a
prior document path and sparse edit maps; it does not copy the 12 GB source or
participate in run creation and merging. An undo-discard option could reduce
retained disk only after repeated structural operations, so presenting it as a
sort accelerator would be misleading.

## Reproduction

```bash
cargo build --release --locked --offline -p quarry-cli --bin quarry-bench
target/release/quarry-bench sort-save-as \
  LARGE_FILE_12GB.csv sorted-12gb.csv \
  --column 2 --order asc --header first-row --cache-state warm
```

For cancellation validation, use an absent destination and add
`--cancel-after-bytes 67108864`.

The 50 GB sort in the capability suite predates this optimization. It remains
valid completion and correctness evidence, but it must be rerun before its
timing can represent the current engine.
