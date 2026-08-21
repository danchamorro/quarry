<p align="center">
  <img src="assets/quarry-wordmark.png" alt="Quarry" width="640">
</p>

# Quarry

> A performance-first, open-source macOS application being built to explore and transform massive delimited text files.

## Built with AI, in the open

Quarry is intentionally built 100% with AI coding agents under human product
direction. That is not a footnote. It is one of the project's defining ideas.
The goal is to demonstrate that agent-built software can be fast, reliable,
accessible, maintainable, and developed with engineering rigor.

AI agents implement, benchmark, test, document, and iterate. The human owner
sets the product vision, evaluates the experience, and accepts the decisions.
The source, architecture decisions, performance evidence, tests, and commit
history are public so the process and results can be inspected.

## Mission
Quarry exists for a simple reason: a file should not become unusable just because it is larger than RAM.

The first objective is deliberately narrow: make a 10 GB CSV practical to open and navigate on a Mac without loading the entire file into memory.

## Core promise
**Open huge delimited files quickly, keep the interface responsive, and make common data operations practical.**

Current alpha capabilities include CSV, TSV, pipe, and semicolon-delimited
files; progressive opening; continuous virtualized rows; resizable columns in a
bounded 32-column display window; direct access to every known column;
view-only hide/show/reorder controls; literal search; bounded cell or row copy;
one or more AND-combined contains/equality filter predicates; and cancellable,
streaming filtered export to a new file. Existing UTF-8 headers and data cells
can be edited directly in the grid, then written through atomic Save with
metadata-based conflict detection or no-clobber Save As. Selected columns can
also be split, combined, moved, or deleted through a private working copy that
reopens as the same editable grid, then edited further or transformed again
before Save or Save As. Literal Find Next sees unsaved cell values, Replace in
Cell edits the current match, and bounded Replace All materializes a private
working copy without changing the source until Save.

One selected numbered column can now sort all data rows by stable,
case-sensitive text in ascending or descending order. The sorted result opens
in the same Modified grid and uses the existing Undo, Redo, Discard, Save, and
Save As workflow.

## Performance direction
The initial reference workload is a **10 GB delimited file**. Quarry should show useful first rows within seconds, keep memory bounded, remain interactive during scans, and avoid full-file copies for read-only work.

## Architecture
The data engine is written in **Rust**. A measured egui/AppKit bake-off selected
**egui** for the production UI while keeping the engine framework-independent.

## First milestone
> Open a 10 GB CSV, display its first rows quickly, and scroll through it smoothly without memory usage scaling with file size.

## Documents
- [PRD](docs/PRD.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Roadmap](docs/ROADMAP.md)
- [Engineering Principles](docs/ENGINEERING_PRINCIPLES.md)
- [Contributing](docs/CONTRIBUTING.md)
- [macOS Packaging and Installation](docs/MACOS_PACKAGING.md)

## Current milestone

The Rust engine and CLI prove progressive opening, correct delimited parsing,
bounded structural indexing, deterministic fixture generation, and row-range
navigation. The Phase 3 egui viewer alpha now has continuous scrolling, native
file opening, drag and drop, delimiter/header controls, and bounded literal
Find Next with progress and cancellation, plus bounded cell and row copying.
It also provides direct access to every known column plus view-only
hide/show/reorder controls. The compact default grid dynamically fits rows to
the available height and keeps at least 40 data rows visible in the maximized
reference window, completing the Phase 3 viewer alpha.

Phase 4 is complete. Filtering combines one or more literal contains or
equality predicates with AND semantics. A background worker parses each row
once, builds a bounded adaptive match index, and serves bounded filtered row
ranges through cancellable reads without retaining every matching row. Safe,
cancellable streaming export passed the 1 GB and 12 GB validation. Phase 5 is
complete. Existing headers and UTF-8 data cells can be edited directly
in the grid. Atomic Save uses metadata-based conflict detection, and no-clobber
Save As leaves the previous source unchanged. Both stream the sparse edit
overlay without loading the file into memory. The direct-cell Save As path is
measured on deterministic 1 GB and 12 GB files. Streaming split and join are
also implemented and validated on both deterministic datasets. The desktop
workflow applies each operation from selected grid columns, reopens the result
as the normal editable working copy, and supports repeated operations. Selected
columns can also move as one block or be deleted explicitly without turning the
view-only Columns manager into an output editor. Find Next searches unsaved cell
values, Replace in Cell changes the current matched cell, and cancellable
Replace All uses the same bounded private rewrite worker and change history.
Phase 6A stable single-column text sorting is complete across the core,
desktop, and validation CLI. Deterministic 1 GB and 12 GB release runs passed
complete order and preservation scans with peak RSS below 20 MiB, measured
temporary disk below the conservative estimate, unchanged source hashes, and
prompt cancellation without leftover files. The worker now also verifies the
effective record multiset and exact stable tie order before publication. Phase
7A desktop packaging is complete. The full installed-app interaction journey
passes, and clean committed build 31 installs as the canonical
`/Applications/Quarry.app`, reports matching source metadata, and enforces the
application/installer exclusion lock. Phase 7B next focuses on polishing current
workflows before the missing-feature inventory.

## Install Quarry

Quit any running Quarry copy, then build, install, and verify the canonical
local alpha application:

```bash
./scripts/macos-app.sh install
./scripts/macos-app.sh verify
open /Applications/Quarry.app
```

The installer preserves any valid prior Quarry app as a rollback archive,
verifies the new bundle before and after replacement, removes any legacy
prototype, and leaves only `/Applications/Quarry.app` active. See the
[macOS packaging guide](docs/MACOS_PACKAGING.md) for package-only, update,
verification, rollback, and signing details.

## Development launch

Run the benchmark-oriented CLI from source:

```bash
cargo run --release -p quarry-cli -- open huge.csv
```

Launch the egui viewer alpha directly from Cargo with or without a path:

```bash
cargo run --release -p quarry-egui
cargo run --release -p quarry-egui -- huge.csv
```

Use **Choose…** for the native macOS picker, drop one local file onto the
window, or type a path and select **Open**. Current delimiter and header
selections apply to newly opened files; changes to the open document wait for
**Apply / Reopen**. After indexing completes, **Find Next** searches decoded
data cells from the first visible data row, including unsaved cell values, and
jumps directly to the matching row and column. Enter replacement text and use
**Replace in Cell** to replace every non-overlapping occurrence in the current
matched cell and continue to the next match. **Replace All** applies the same
literal, case-sensitive replacement across every data cell after applying
unsaved cell edits. It reports progress, can be cancelled, skips the header,
and reopens a successful result as an unsaved private working copy. No match,
cancellation, or failure leaves the document unchanged.

Use **Filters…** to choose a one-based file column and a literal, case-sensitive
**Contains** or **Equals** predicate. Select **Add AND rule** to combine more
rules; a row appears only when every rule matches. The grid shows matching rows
while the background scan progresses. Page Up/Page Down, wheel, and scrollbar
navigation operate on those matches. **Cancel filter** stops the scan, and
**Clear filter** returns to the full file. Value editors accept literal newlines
for matching multiline fields.

After filtering completes, use **Export Filtered Rows…** to choose a new output
file. Quarry copies the original header and matching records without changing
the source, reports progress, and supports cancellation without publishing a
partial destination.

Double-click an existing data cell, or select it and press **Enter** or **F2**,
to edit it in the grid. Press **Shift+Enter** to insert a newline, **Enter** to
commit the in-memory change, or **Escape** to cancel the active edit. Quarry
does not modify the file until **Save** or **Save As…** succeeds. Missing cells
in ragged rows and invalid UTF-8 cells remain non-editable. Clear an active
filter before editing. Find and Replace use current unsaved cell values;
filtering still requires data-cell edits to be saved or discarded because the
filter worker scans the active indexed CSV.

Plain-click a column number to select one column. **Shift-click** another number
to select a contiguous range of shown columns, or **Command/Ctrl-click** to
toggle separate columns. Selected columns stay visibly highlighted through
their numbered header, name, and loaded cells. Right-click a selected number to
open its context menu and choose **Split Columns…**. The compact dialog starts
with that column selected; enter a non-empty literal separator and choose
**OK**. Quarry derives the required width from the document data, keeps the
original header on the first result, adds blank editable headers for additional
results, and renumbers later columns to their new document positions. If the
separator does not occur, Quarry leaves the document unchanged. Select two or
more numbered columns to use
**Combine Columns…** with an optional literal separator. The joined value
replaces the selected columns at their leftmost document position and keeps
that position's current header.

**OK** runs the operation in the background, including a cancellable data scan
to determine Split width, and streams the current document plus sparse edits to
a private working CSV. Quarry then reopens that result as the normal editable
grid. Continue editing cells or headers, or apply Split, Combine, Move, and
Delete in any order. **Cancel** leaves the document unchanged, and one-level
structural Undo and Redo move between adjacent document versions. The source
file is not modified until **Save** succeeds; **Save As…** writes and opens a new
file while preserving the previous source, and **Discard Changes** removes the
private working copies and restores the last opened or saved file. Quarry
displays at most 32 document columns per viewport and applies the core
65,536-column structural safety limit. Persistence is not otherwise limited by
the bounded viewport.

Right-click a selected numbered header and choose **Move Selected Columns…** to
move every selected column as one block. Enter the one-based destination for
the block's first output position after Quarry removes it from the current
order, then choose **Move**. Unselected columns retain their order, and moving a
block to its current position changes nothing. **Delete Selected Columns**
starts without a dialog; Quarry prevents deleting every known column and
selects the nearest survivor. Hidden columns remain in the file; to delete one,
show it and select it explicitly. Both actions preserve
later undiscovered fields in ragged rows and use the same background progress,
cancellation, Undo/Redo, Discard, Save, and Save As workflow as Split and
Combine.

Select exactly one numbered header and choose **Sort Rows…** to order every
data row by that column. Choose ascending or descending in the compact dialog.
Comparison is case-sensitive text, missing ragged fields compare as empty, the
header stays fixed, and equal values retain their current order. Quarry waits
for indexing to finish so it can show a conservative temporary-disk allowance
before Sort is enabled. A successful result immediately replaces the grid as a
Modified private working copy; cancellation or failure leaves the document
unchanged.

Reproduce a filtered export from the CLI without loading the entire source or
retaining all matches in memory:

```bash
cargo run --release -p quarry-cli --bin quarry-bench -- export huge.csv \
  --output filtered.csv --column 1 --operator equals --value example \
  --cache-state unknown
```

Measure streaming Save As with deterministic sparse cell edits and automatic
read-back validation:

```bash
cargo run --release -p quarry-cli --bin quarry-bench -- edit-save-as huge.csv \
  --output edited.csv \
  --edit 1 1 first-value \
  --edit 1000000 6 middle-value \
  --edit 5000000 11 deep-value \
  --cache-state unknown
```

Measure the streaming persistence engine with exact header, record-count,
schema, and first/middle/final row validation. These benchmark commands use one
deterministic operation per run; they are persistence evidence rather than the
desktop interaction contract. Move and Delete reuse this measured bounded
worker; exact core regressions cover their ordering, sparse edits, ragged rows,
and source preservation without adding another benchmark-only CLI mode.
Replace All also reuses the private rewrite worker measured here; exact core
and desktop regressions cover its overlay-first replacement, no-match,
cancellation, cleanup, and Undo behavior without claiming separate 1 GB or
12 GB Replace All timings:

```bash
cargo run --release -p quarry-cli --bin quarry-bench -- transform-save-as huge.csv \
  --output split.csv --split 1 , 2 \
  --output-header column_1_prefix --output-header column_1_suffix \
  --cache-state warm

cargo run --release -p quarry-cli --bin quarry-bench -- transform-save-as huge.csv \
  --output joined.csv --join 3,4 '|' --output-header column_3_4 \
  --cache-state warm
```

Exercise stable text sorting through the guarded owner-only validation
artifact and automatic read-back check:

```bash
cargo run --release -p quarry-cli --bin quarry-bench -- sort-save-as \
  huge.csv sorted.csv --column 1 --order asc --header first-row \
  --cache-state unknown
```

Use **Columns…** to view or hide a one-based file column, move it to any display
position, drag it by its handle inside the Columns window, or reset the layout.
Hidden columns remain part of that display order. The main grid headers stay
resize-only, which prevents accidental reordering while browsing. Each header
shows its stable, one-based document-column number above the column name. A
Split or Combine command creates a new current document schema and renumbers
affected positions. Move and Delete also create a new document schema, while
later view-only hide and reorder actions preserve those new identities for
search and copy. Quarry renders at most 32 data columns at once.
Header columns are known immediately; extra fields in later ragged rows are
appended when Quarry encounters them.

Click a cell or its row number, then use **Copy** or **Command+C**. Cell copy
preserves the complete decoded value. Row copy emits every actual field as
UTF-8 TSV, excluding the header and synthetic row number, with a 64 MiB
clipboard limit.

The measured native comparator remains runnable for bake-off reproduction:

```bash
cargo run --release -p quarry-appkit -- huge.csv
```

Request a row range with `--jump`; Quarry serves it as soon as background
indexing reaches that range, then continues indexing:

```bash
cargo run --release -p quarry-cli -- open huge.csv \
  --jump 100000000 --jump-count 3
```

Measure repeated, sequential, and deterministic random viewport reads:

```bash
cargo run --release -p quarry-cli --bin quarry-bench -- viewport huge.csv \
  --iterations 500 --rows 100 --seed 1 --cache-state warm
```

Measure a complete bounded literal search without retaining a results list:

```bash
cargo run --release -p quarry-cli --bin quarry-bench -- search huge.csv \
  --query QUARRY_NO_MATCH_9F7B2C --cache-state unknown
```

Measure a complete two-predicate AND scan and bounded filtered-row reads:

```bash
cargo run --release -p quarry-cli --bin quarry-bench -- filter huge.csv \
  --column 1 --operator contains --value 'with "quotes"' \
  --and 2 equals $'line one\nline two' \
  --cache-state unknown
```

Generate a deterministic local fixture:

```bash
cargo run --release -p quarry-cli --bin quarry -- generate \
  --size 10GB --columns 40 --delimiter , \
  --output fixtures/generated/test-10gb.csv --seed 1
```

See the [12 GB engine benchmark](docs/benchmarks/2026-08-14-large-file.md),
[egui spike results](docs/benchmarks/2026-08-14-egui-spike.md),
[AppKit spike results](docs/benchmarks/2026-08-14-appkit-spike.md),
[continuous-scroll results](docs/benchmarks/2026-08-15-continuous-scroll.md),
[viewer file-controls validation](docs/benchmarks/2026-08-15-viewer-file-controls.md),
[live-index latency results](docs/benchmarks/2026-08-15-live-index-latency.md),
[streaming-search results](docs/benchmarks/2026-08-15-streaming-search.md),
[bounded-copy validation](docs/benchmarks/2026-08-16-bounded-copy.md),
[column-controls validation](docs/benchmarks/2026-08-16-column-controls.md),
[row-density validation](docs/benchmarks/2026-08-16-row-density.md),
[streaming-filter validation](docs/benchmarks/2026-08-16-streaming-filter.md),
[multiple-predicate filter validation](docs/benchmarks/2026-08-16-multiple-predicate-filter.md),
[filtered-export validation](docs/benchmarks/2026-08-16-filtered-export.md),
[direct-cell editing validation](docs/benchmarks/2026-08-18-direct-cell-editing.md),
[split/join transformation validation](docs/benchmarks/2026-08-19-split-join-transformations.md),
[Phase 6A stable-text-sort validation](docs/benchmarks/2026-08-21-stable-text-sort.md),
[initial engine decision](docs/adr/0001-initial-engine.md),
[viewport cache decision](docs/adr/0002-defer-viewport-cache.md), and
[UI decision](docs/adr/0003-select-egui-ui.md).

Quarry is dual-licensed under MIT or Apache-2.0.
