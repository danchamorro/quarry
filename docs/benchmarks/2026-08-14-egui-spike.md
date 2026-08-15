# egui UI spike — 2026-08-14

## Scope

This is the first Phase 2 UI candidate, not a framework decision. It reuses the
existing `Session`, `IndexJob`, and `StructuralIndex` paths and materializes at
most 100 rows for each viewport request.

The workspace uses `eframe` and `egui_extras` 0.33.3 because Quarry currently
declares Rust 1.88 compatibility and
[eframe 0.33.3 supports Rust 1.88](https://docs.rs/crate/eframe/0.33.3).

## 12 GB smoke

Environment and dataset match the [engine benchmark](2026-08-14-large-file.md).
The release binary was launched with:

```bash
/usr/bin/time -l target/release/quarry-egui LARGE_FILE.csv
```

| Metric | Result |
|---|---:|
| First UI update, including file open and first viewport | 187.423 ms |
| RSS while background indexing was active | 136.9 MiB |
| Maximum resident set size | 137.0 MiB |
| Peak physical footprint reported by macOS | 369.2 MiB |
| Background index worker | Idle by the 33-second observation |

The run stayed below the initial 500 MiB memory target. The index-completion
observation is deliberately coarse; the existing engine benchmark remains the
source for precise indexing throughput.

## Interaction and accessibility smoke

A 1.91 MiB equivalent fixture with 11 columns and 10,221 data rows was used for
repeatable macOS UI inspection. The full-height grid exposed its controls,
column headers, row numbers, and visible cells through AccessKit.

- Next moved from rows 1–100 to 101–200 in 2.013 ms.
- Jump moved to rows 500–599 in 3.298 ms.
- Page Down moved to rows 600–699 in 2.503 ms.
- The focused regression test verifies one-based row mapping and the 100-row cap.

The temporary test application bundle could not read the file in `Documents`
without a macOS privacy grant, so the raw release binary was used for the real
12 GB run. No permission was granted or changed.

## Decision

The equivalent [AppKit spike](2026-08-14-appkit-spike.md) completed the bake-off.
[ADR 0003](../adr/0003-select-egui-ui.md) selects egui for the production UI.
