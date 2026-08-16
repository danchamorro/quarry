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
view-only hide/show/reorder controls; literal search; and bounded cell or row
copy. Filtering, transformations, and streaming export remain planned.

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

## Current milestone

The Rust engine and CLI prove progressive opening, correct delimited parsing,
bounded structural indexing, deterministic fixture generation, and row-range
navigation. The Phase 3 egui viewer alpha now has continuous scrolling, native
file opening, drag and drop, delimiter/header controls, and bounded literal
Find Next with progress and cancellation, plus bounded cell and row copying.
It also provides direct access to every known column plus view-only
hide/show/reorder controls. The compact default grid dynamically fits rows to
the available height and showed 42 data rows in the maximized reference window,
completing the Phase 3 viewer alpha.

```bash
cargo run --release -p quarry-cli -- open huge.csv
```

Launch the egui viewer alpha with or without a CLI path:

```bash
cargo run --release -p quarry-egui
cargo run --release -p quarry-egui -- huge.csv
```

Use **Choose…** for the native macOS picker, drop one local file onto the
window, or type a path and select **Open**. Current delimiter and header
selections apply to newly opened files; changes to the open document wait for
**Apply / Reopen**. After indexing completes, **Find Next** searches decoded
cells from the first visible data row and jumps directly to the matching row
and column.

Use **Columns…** to view or hide a one-based file column, move it to any display
position, drag it by its handle inside the Columns window, or reset the layout.
Hidden columns remain part of that display order. The main grid headers stay
resize-only, which prevents accidental reordering while browsing. Quarry
renders at most 32 data columns at once while keeping source column identity
stable for search and copy. Header columns are known immediately; extra fields
in later ragged rows are appended when Quarry encounters them.

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
[initial engine decision](docs/adr/0001-initial-engine.md),
[viewport cache decision](docs/adr/0002-defer-viewport-cache.md), and
[UI decision](docs/adr/0003-select-egui-ui.md).

Quarry is dual-licensed under MIT or Apache-2.0.
