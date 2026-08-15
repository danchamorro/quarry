# AppKit UI spike — 2026-08-14

## Scope

This is the native Phase 2 comparator. It uses `objc2-app-kit` 0.3.2 and the
same `Session`, `IndexJob`, and `StructuralIndex` paths as the egui candidate.
Each viewport request materializes at most 100 rows.

The prototype uses native controls around a read-only, scrollable monospaced
`NSTextView`. It does not attempt a production `NSTableView` or custom grid.

## 12 GB smoke

Environment and dataset match the [engine benchmark](2026-08-14-large-file.md).
The release binary was launched with:

```bash
/usr/bin/time -l target/release/quarry-appkit LARGE_FILE.csv
```

| Metric | Result |
|---|---:|
| First window, including file open and first viewport | 151.908 ms |
| RSS while background indexing was active | 111.5 MiB |
| Maximum resident set size | 114.1 MiB |
| Peak physical footprint reported by macOS | 43.0 MiB |
| Background index worker | Idle by the 28-second observation |

The run stayed below the initial 500 MiB memory target. The process was stopped
after indexing went idle, so the 61.21-second wall time reported by `time`
includes about 33 seconds of idle observation. The engine benchmark remains the
source for precise indexing throughput.

## Interaction, scrolling, and accessibility smoke

The same 1.91 MiB fixture used for the egui smoke contains 11 columns and 10,221
data rows.

- Next moved from rows 1–100 to 101–200 in 3.544 ms.
- Jump moved to rows 500–599 in 3.453 ms.
- Page Down moved to rows 600–699 in 1.573 ms.
- Page Up returned to rows 500–599 in 1.179 ms.
- A 10-page native scroll burst completed in 1.026 seconds and reached the end
  of the bounded viewport without a visible stall.
- The focused regression test verifies one-based row mapping and the 100-row
  cap.

File, progress, cancellation, navigation, jump, and scroll controls are exposed
through macOS accessibility. The data viewport is one text area, however, so it
does not provide the per-header and per-cell granularity of the egui candidate.

## Integration cost

| Measure | AppKit | egui |
|---|---:|---:|
| Release binary | 0.57 MiB | 5.18 MiB |
| Normal dependency packages | 17 | 273 |
| Prototype source | 933 lines | 669 lines |
| Source lines containing `unsafe` | 21 | 0 |

The AppKit candidate is materially smaller and lighter. Direct Rust integration
requires Objective-C selectors, lifecycle management, feature-gated bindings,
and unsafe calls, while its text viewport would still need replacement with a
cell-aware grid for production accessibility.

The comparable 10-page egui scroll burst completed in 1.073 seconds. These
automation timings include a roughly one-second synthesized event stream and
are useful as a stall check, not as renderer frame-time measurements.

