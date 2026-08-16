# Live-index viewport latency — 2026-08-15

## Decision

Use a 1 MiB default structural-index chunk instead of 8 MiB. Across a balanced
ABBAAB comparison on the 11.33 GiB reference file, the median live viewport p95
for the combined snapshot-and-read path fell from 15.302 ms to 3.540 ms while
median indexing throughput increased from 560.96 MiB/s to 576.73 MiB/s.

## Why this was measured

The indexer held the structural-index write lock while parsing each 8 MiB chunk.
The egui live viewport path waits for that lock, clones the current index, and
then reads the requested rows. Two Computer Use Page Down reads during indexing
had taken 10.565 ms and 12.809 ms, compared with 6.408 ms after indexing.

The benchmark times the actual caller-visible operations separately:

- `snapshot`: read-lock wait plus structural-index clone;
- `row read`: materializing 100 sequential rows from that snapshot;
- `combined`: the end-to-end snapshot plus row-read path used by the viewer.

It schedules 1,000 requests at 16 ms intervals. A request whose deadline has
already passed is skipped instead of being issued in a catch-up burst. This
keeps the request ceiling and 16-second wall-clock schedule comparable between
variants.

## Environment

- Apple M3 Max MacBook Pro, 16 CPU cores, 128 GiB RAM
- macOS 26.6.1 (25G76), arm64
- Rust 1.88.0
- working-tree release build based on commit `1ad06aa`
- `LARGE_FILE.csv`: 12,167,847,982 bytes (11.33 GiB), warm cache
- 117,168,830 indexed records including the header record
- 100 rows per request, 1,000 scheduled requests, 16 ms interval

An unrecorded full scan established the declared warm state. It completed in
19.647 seconds at 590.64 MiB/s.

## Reproduction

```bash
cargo build --release -p quarry-cli

target/release/quarry-bench open LARGE_FILE.csv \
  --rows 100 --cache-state warm

target/release/quarry-bench viewport LARGE_FILE.csv --live \
  --iterations 1000 --rows 100 --interval-ms 16 \
  --chunk-bytes 8388608 --cache-state warm

target/release/quarry-bench viewport LARGE_FILE.csv --live \
  --iterations 1000 --rows 100 --interval-ms 16 \
  --chunk-bytes 1048576 --cache-state warm
```

The recorded order was 8 MiB, 1 MiB, 1 MiB, 8 MiB, 8 MiB, 1 MiB.

## Results

| Run | Chunk | Completed | Snapshot p95 | Row-read p95 | Combined p95 | Missed deadlines | Combined >16 ms | Throughput | Index time | Peak RSS |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A1 | 8 MiB | 862 | 12.585 ms | 3.052 ms | 15.302 ms | 138 | 21 | 560.78 MiB/s | 20.693 s | 15.44 MiB |
| B1 | 1 MiB | 999 | 1.590 ms | 2.000 ms | 3.473 ms | 1 | 1 | 589.66 MiB/s | 19.679 s | 8.03 MiB |
| B2 | 1 MiB | 999 | 1.629 ms | 2.123 ms | 3.550 ms | 1 | 1 | 576.47 MiB/s | 20.130 s | 7.67 MiB |
| A2 | 8 MiB | 886 | 12.583 ms | 2.971 ms | 15.292 ms | 114 | 13 | 560.96 MiB/s | 20.686 s | 14.73 MiB |
| A3 | 8 MiB | 880 | 12.656 ms | 2.909 ms | 15.310 ms | 120 | 13 | 561.39 MiB/s | 20.671 s | 14.81 MiB |
| B3 | 1 MiB | 998 | 1.614 ms | 2.055 ms | 3.540 ms | 2 | 0 | 576.73 MiB/s | 20.121 s | 7.70 MiB |

| Median | 8 MiB | 1 MiB | Change |
|---|---:|---:|---:|
| Snapshot p95 | 12.585 ms | 1.614 ms | 87.2% lower |
| Row-read p95 | 2.971 ms | 2.055 ms | 30.8% lower |
| Combined p95 | 15.302 ms | 3.540 ms | 76.9% lower |
| Missed deadlines | 120 | 1 | 99.2% lower |
| Combined reads over 16 ms | 13 | 1 | 92.3% lower |
| Indexing throughput | 560.96 MiB/s | 576.73 MiB/s | 2.8% higher |
| Indexing time | 20.686 s | 20.121 s | 2.7% lower |
| Peak RSS | 14.81 MiB | 7.70 MiB | 48.0% lower |

The sampling windows started at 0.14% indexed for 8 MiB and 0.09% for 1 MiB;
they ended at 76.94–77.70% and 79.42–81.44%, respectively. Both variants
produced 117,168,830 indexed records and a 446.97 KiB structural index. B1 and
B2 each completed 999 requests and produced the same checksum, `5368410`.

## Acceptance

The 1 MiB candidate passes the predeclared gates:

- combined p95 is below 8 ms and improved by more than 20%;
- indexing throughput is above 95% of the 8 MiB baseline;
- peak memory remains far below 500 MiB;
- row counts and index size remain unchanged.

A final release build using the new default, without `--chunk-bytes`, completed
999 of 1,000 scheduled requests with one missed deadline and no combined reads
over 16 ms. Its p95 was 1.623 ms for snapshot, 2.028 ms for row read, and 3.518
ms combined; the full index completed in 20.053 seconds at 578.68 MiB/s with
9.50 MiB peak RSS.

No timing assertion was added to CI because scheduler load and filesystem cache
would make it flaky. Existing scanner tests exercise every split point in a
representative quoted record, and core tests cover multi-chunk indexing and
coherent live snapshots.

## Limits

The sequential rows are intentionally hot: this isolates structural-index lock
contention rather than claiming general random or frontier-read performance.
`snapshot` includes both lock wait and checkpoint-vector cloning because that is
the cost visible to the UI caller. The balanced repeated runs reduce, but do not
eliminate, operating-system scheduling and cache noise.
