# Column controls validation

## Decision

Ship direct access to every known source column with view-only hide/show,
arbitrary move-to-position, manager-only drag reorder, first-columns, and reset
actions. Main-grid headers remain resize-only. The grid still renders at most
32 data columns, search reveals hidden matches, and row copy preserves the
original file order.

Column layout metadata scales with known column count, not file size. Header
columns are known immediately. Extra fields in ragged rows are appended when a
loaded viewport or search result discovers them; Quarry does not add a separate
full-file schema scan for this feature.

## Environment and datasets

- Date: 2026-08-16
- Hardware: Apple M3 Max MacBook Pro, 16 CPU cores, 128 GB RAM
- OS: macOS 26.6.1 (25G76), arm64
- Rust: 1.88.0
- Build: `cargo build --workspace --release`
- Cache state: unknown for both UI runs
- Wide fixture header mode: Auto, detected as a header row
- Wide fixture: 1,000,404 bytes, 40 columns, 1,452 logical data rows, seed 40
- Wide fixture SHA-256:
  `ac00fa3917a21b457cdb2fafc9ab7ba843334fdb946519644768918d85d84ef2`
- Reference file: `LARGE_FILE.csv`, 12,167,847,982 bytes (11.33 GiB), modified
  2026-08-14 19:00:00 EDT
- Reference rows: 117,168,829 data rows after the header

The wide fixture was reproduced with:

```bash
target/release/quarry generate \
  --size 1MB --columns 40 --delimiter , \
  --output /private/tmp/quarry-column-controls-40.csv --seed 40
```

## Results

Computer Use exercised the release build through a temporary local macOS app
bundle. The 12 GB file was selected through the native picker so the temporary
bundle received file access.

| Check | Result |
|---|---|
| Initial wide view | 32 of 40 columns rendered; first rows measured 3.952 ms. |
| Direct access | Viewing file column 40 changed the bounded window to columns 9 through 40. |
| Hide/show | Hiding column 40 changed the manager from 40 shown to 39; View restored it to 40 and centered it. |
| Move to position | Moving file column 40 to display position 2 produced `column_1`, `column_40`, `column_2`. |
| Drag reorder | Dragging column 1 below column 5 produced `column_40`, `column_2`, `column_3`, `column_4`, `column_5`, `column_1`, `column_6`. |
| Grid safety | Dragging a main-grid header did not change the column order; header dragging remains confined to resize boundaries. |
| Reset | Reset columns restored `column_1`, `column_2`, `column_3` and all 40 shown. |
| Accessibility | Both numeric fields were labelled; View, Hide, Move, Reset, checkboxes, and per-column actions were exposed through AccessKit. Pointer-only drag handles were hidden from AccessKit in favor of the exact Move control. |
| 12 GB progressive open | Useful rows appeared while indexing was at 24.8%; the bounded first-row read measured 4.544 ms. |
| 12 GB completion | Indexing reached 100% and 117,168,829 data rows; all 11 source columns remained available in the manager. |
| 12 GB current RSS | 137,424 KiB (134.2 MiB) after indexing, below the 500 MiB viewing target. |

The RSS sample is a completed-viewer observation, not a new peak-memory or
cold-cache claim. Existing engine evidence remains the basis for file-size
independent indexing and viewport memory.

## Automated coverage

The 20 `quarry-egui` tests cover:

- one-based column parsing, direct viewing, and the 32-column render cap;
- hide/show, arbitrary reorder, drag insertion math, first-columns, and reset behavior;
- source-column identity for cell selection, search reveal, and copy;
- original field order for row copy after hide/reorder;
- hidden search matches becoming visible and centered;
- later ragged columns appending without resetting layout;
- successful document replacement resetting layout and failed open preserving it;
- all data columns hidden without losing row selection access;
- accessible manager actions for an offscreen column, checkbox changes, Enter,
  and Command+C focus protection.

The full workspace validation passed 44 tests, strict Clippy with warnings
denied, formatting, a release build, and diff checks.

## Limits

This slice does not persist layouts, auto-scroll a drag across manager rows that
are not currently visible, freeze columns, or scan an entire headerless ragged
file solely to discover its maximum width. The position field provides direct
long-distance moves. Default row density remains unchanged and is the next
Phase 3 task.
