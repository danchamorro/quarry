# 12 GB progressive-open benchmark — 2026-08-14

## Environment

- Apple M3 Max, 16 CPU cores, 128 GiB RAM
- macOS 26.6.1 (25G76)
- Rust 1.88.0 stable, release profile with thin LTO
- Dataset: `LARGE_FILE.csv`, 12,167,847,982 bytes (11.33 GiB), 11 columns
- CSV records: 117,168,830, including the header
- The file ends in LF. EmEditor reports 117,168,831 text lines because it
  displays the empty line after the trailing terminator; Quarry counts CSV
  records.

## Quarry

Command:

```bash
target/release/quarry-bench open LARGE_FILE.csv \
  --jump 100000000 --jump-count 3 --cache-state warm
```

The run was warm because EmEditor had just read the full file. No attempt was
made to purge macOS file caches.

| Metric | Result |
|---|---:|
| Time to open file | 0.042 ms |
| Time to 100 correctly parsed rows | 4.828 ms |
| Bootstrap data read | 1.00 MiB |
| Current RSS at first rows | 11.97 MiB |
| Full structural index | 21.521 s |
| Index throughput | 539.20 MiB/s |
| Index checkpoints | 28,606 at 4,096-row intervals |
| Index allocation | 446.97 KiB |
| Jump to row 100,000,000 (3 rows) | 1.956 ms |
| Peak RSS | 13.92 MiB |

This clears the initial targets of under 3 seconds to useful rows and under
500 MiB RAM. The 16 MiB adaptive checkpoint budget prevents index memory from
growing without bound on larger files.

### Live navigation and cancellation follow-up

After live-index navigation was connected to the CLI, the same warm 12 GB
workload produced:

| Metric | Result |
|---|---:|
| Time to 100 correctly parsed rows | 9.246 ms |
| Live jump to row 100,000,000 | 17.894 s after indexing began |
| Index completion at live jump | 85.76% |
| Live row-range read (3 rows) | 2.895 ms |
| Full structural index | 20.862 s |
| Index throughput | 556.22 MiB/s |
| Peak RSS | 12.05 MiB |
| Cancellation after first viewport | 15.160 ms |

The cancellation run stopped after the current 8 MiB chunk. The targeted core
regression also verifies partial-index reads, the explicit not-indexed-yet
boundary, and worker cancellation before the test file finishes indexing.

## EmEditor comparison

EmEditor ran under Windows 11 through Parallels Desktop for Mac Pro 20.1.3.
The file was opened from Finder with EmEditor before Quarry's full-file run, so
its OS cache state is unknown rather than confirmed cold.

Computer Use observations from the click that opened the file:

- At 0.333 s, EmEditor still showed the prior untitled window.
- By 5.763 s, rows were visibly usable while the status bar reported 3,070 MB
  of 11,604 MB read (26%, 31,048,558 lines, 754 MB/s).
- At 14.992 s, it reported 93% read and one second remaining.
- At 35.842 s, syntax validation was complete. EmEditor reported no errors and
  31.570 s for its one-thread CSV syntax check at 367 MB/s.

The first-viewport figure is a measured upper bound, not an exact event time,
because UI readiness was sampled. The comparison is directional rather than
strictly apples-to-apples: EmEditor used a Windows compatibility environment,
while Quarry ran natively and after the file had been warmed by EmEditor.

## Reproducibility note

For a future cold-cache comparison, use a fresh restart or a dataset the OS has
not read, run each tool in alternating order, and repeat multiple times. Do not
label a run cold merely because it is the first application-level open.
