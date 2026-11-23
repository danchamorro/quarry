# Quarry GPUI comparison

A working comparison of the compact Quarry design using GPUI Kit 0.10.0,
`quarry-workbench`, and the existing `quarry-core` engine. The production egui
app (`apps/quarry-egui`) remains the default packaged macOS target until the
Phase 4B-3 cutover; this workspace member is used for GPUI hardening,
accessibility/platform verification, and progressive feature parity.

## Run

```sh
cargo run -p quarry-gpui --release --locked
# Or open a file at startup:
cargo run -p quarry-gpui --release --locked -- /absolute/path/to/file.csv
```

This crate is a member of the root Cargo workspace and shares the root
`Cargo.lock` and pinned Rust 1.100.0 toolchain (`rust-toolchain.toml`). Commands
can be run from the repository root (`-p quarry-gpui`) or from `apps/quarry-gpui`.
The production macOS `.app` bundle (`scripts/macos-app.sh`) still packages
`apps/quarry-egui` until Phase 4B-3.

## Try

- Open a CSV or other delimited file through the native picker, or press
  Cmd+O (macOS) / Ctrl+O (Linux and Windows).
- Scroll vertically and horizontally. Drag column boundaries to resize.
  Home/End navigate to the first/last column. Native trackpad horizontal
  scrolling remains a manual comparison check; see `VALIDATION.md`.
- Use Row and Go to jump to an indexed row (a match position when filtering).
- Open Columns to search, hide, reorder, or Auto-fit columns.
- Open Filters to enter literal text or numeric rules and apply them through
  Quarry's engine. Between includes both endpoints. Rules retain source column
  identities even when columns are hidden or reordered.
- Open Format to change delimiter or header detection and reopen the file.
- Double-click a data cell, or select it and press F2, to edit its full value.
  The editor supports line breaks and retains your draft if an edit fails.
  Apply the edit, then try Undo and Redo.
- Save a copy with Cmd+S (macOS) / Ctrl+S (Linux and Windows). This creates a
  new file containing all rows and edits; it never overwrites the source or an
  existing destination. Open the new copy to verify it. Saving a copy leaves the
  original session and its edits intact.

The title and footer identify this executable as the GPUI comparison. It uses
Kit's styled inputs, selectors, popovers, buttons, and virtual table, with
Quarry's pale surfaces, compact spacing, amber actions, and monospace grid.

## Scope and limits

The comparison includes opening, indexing, both-axis virtual scrolling, compact
Columns/Filters/Format panels, cell editing, undo/redo, and Save a Copy. It does
not yet reproduce production Find, clipboard export, structural operations,
sorting, filtered export, settings, recovery, or accessibility feature parity.
Do not use it as a replacement for the beta app.

- File work and job destruction run on a dedicated worker. Scroll requests are
  coalesced, and replies from superseded views are ignored.
- The display cache holds at most 256 rows and 64 source columns; displayed
  values are clipped to 512 bytes. Opening the cell editor reads the full UTF-8
  value, up to 1 MiB. Invalid UTF-8 remains viewable but cannot be edited.
- The engine still parses every field in requested records. Display projection
  is not a raw-parser memory cap: wide records, its 64 MiB per-record limit,
  bootstrap data, indexes, and sparse edits also contribute to memory use.
- Undo/redo history has the same 1,000-entry and 16 MiB limits as the egui app.
  Sparse edited values remain separate from history.
- Auto-fit estimates widths from headers and the current cached rows; it does
  not scan the whole file. Other columns can still be resized manually.
- Filtering uses source bytes. Undo edits or save and open a copy before
  filtering; clear the filter before editing or saving a complete copy.
- The window close control and Quit command ask before discarding edits.
  This prototype has no crash recovery or automatic saving.

## Validation

```sh
cargo fmt --all -- --check
cargo clippy -p quarry-gpui --all-targets --locked -- -D warnings
cargo test -p quarry-gpui --locked
cargo build -p quarry-gpui --release --locked
```

Tests exercise the real engine on wide data, bounded display requests, source
identity, quoted multiline edits, undo/redo limits, new-file save safety,
source-change detection, filter semantics and validation, and worker response
ordering. Native comparison observations belong in `VALIDATION.md`; compilation
alone does not establish that GPUI is faster or justify a migration.

GPUI Kit and its GPUI implementation are Apache-2.0. No telemetry or application
network calls are added. This experiment is not part of the production package
or its license inventory. A distributed GPUI build would require its own full
transitive dependency and notice review.
