# Quarry

> A performance-first, open-source macOS application for exploring and transforming massive delimited text files.

## Built with AI, in the open

Quarry is intentionally built 100% with AI coding agents under human product
direction. That is not a footnote—it is one of the project's defining ideas.
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

Initial capabilities: CSV/TSV/pipe-delimited files, progressive opening, virtualized rows/columns, search, filtering, column operations, split/join transformations, and safe streaming export.

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
Find Next with progress and cancellation.

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
[streaming-search results](docs/benchmarks/2026-08-15-streaming-search.md),
[initial engine decision](docs/adr/0001-initial-engine.md),
[viewport cache decision](docs/adr/0002-defer-viewport-cache.md), and
[UI decision](docs/adr/0003-select-egui-ui.md).

Quarry is dual-licensed under MIT or Apache-2.0.
