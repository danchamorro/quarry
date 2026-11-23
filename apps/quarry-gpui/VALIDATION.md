# GPUI comparison validation

Local development comparison, September 10, 2026. This is evidence for trying
the design, not a migration or release decision.

## Environment and inputs

- Apple M3 Max, 128 GiB memory, macOS 26.6.2 (25G83).
- GPUI Kit 0.6.1, Rust 1.97.1, optimized release build.
- Existing `quarry-core` is used through a path dependency without changes.
- Synthetic large file: 1,000,000 data rows, 42 columns, 276,689,405 bytes.
- Synthetic wide file: 10,000 data rows, 300 columns, 31,601,501 bytes.
- Fixtures and the local app wrapper are in the repository's ignored `target/`
  directory. No customer records are needed for these checks.

## Automated checks

Run from `apps/quarry-gpui`:

```sh
cargo fmt -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Final result: formatting, strict Clippy, all 10 tests, and the release build pass.

The tests exercise engine parsing, bounded viewport requests, source row and
column identity, filter semantics, full-value editing, undo/redo limits, source
change rejection, safe copy creation, and worker response ordering. Real GPUI
root/table/dialog tests also dispatch horizontal wheel input, double-click,
typing, Apply, and Undo. They verify the edited output and unchanged source,
reachable editor controls, oversized-input feedback, initial shortcuts, and
preservation of the typed draft when the engine rejects a changed source file.
The shortcut regression also covers restoring focus after a panel blurs.

The dependency `block 0.1.6` produces a future Rust compatibility notice. It does
not fail the current build or strict Clippy checks.

## Native Mac observations

- Opened both files through the native file picker and completed indexing.
- Format's Header choice reopened the large file with its first row as headers,
  leaving exactly 1,000,000 data rows. Auto detection originally chose no header,
  matching the engine's behavior on this synthetic input.
- Jumped near the end of the million-row file and displayed the final rows.
- Applied `State contains NV` and observed exactly 500,000 of 1,000,000 rows
  with `Filter complete`. Clearing restored all 1,000,000 rows from the top.
- Scrolled the wide file vertically. Home/End navigation reached the first and
  300th columns with the correct cell values and fixed source row numbers.
- Columns hid a source column, reordered adjacent columns, and auto-fitted the
  live table. Source values moved with the intended column.
- Edited `r1-c1` to `GPUI native check` in the native cell dialog, saved a new
  `wide-native-check.csv` through the native Save dialog, and verified the value
  on disk. The original `wide.csv` still contained `r1-c1`. Undo restored the
  grid and removed its dirty indicator.
- Reopened the saved copy in the final release build and saw the persisted
  edited value. The comparison was then returned to its empty opening view.
- Cmd+O worked immediately after launch. The close guard offered Cancel and
  Discard for an unsaved editor draft.
- Inspected the compact Format and Filters layouts after fixing selector
  sizing, dialog rendering, and shared theme tokens. Apply uses Quarry's amber
  accent.
- Confirmed Columns rows fill the panel with reorder arrows aligned at right.

## Remaining comparison work

- Horizontal pixel-wheel input passes the real GPUI UI test. Native automation's
  horizontal scroll command did not move the table, although keyboard navigation
  reached column 300. Physical trackpad/mouse horizontal scrolling still needs
  a human check; this result does not establish whether the limitation is in
  the automation or native event handling.
- A nested chooser can consume the first outside click: switching directly to
  another toolbar panel may take a second click. This interaction needs polish
  before considering migration.
- No controlled egui-versus-GPUI frame-time, startup-time, or peak-memory
  benchmark was performed. These checks do not establish a speed advantage.
- Full keyboard and screen-reader parity, crash recovery, and the production
  operations listed in README remain outside this comparison.

The installed `/Applications/Quarry.app` and production Cargo workspace were
not changed. The comparison wrapper is local and ad-hoc signed; it is not a
notarized beta release or a package for distribution.
