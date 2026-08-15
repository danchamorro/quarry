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
navigation. Phase 2 selected egui; Phase 3 will turn that candidate into the
viewer alpha.

```bash
cargo run --release -p quarry-cli -- open huge.csv
```

Launch the selected egui prototype (it is not the viewer alpha yet):

```bash
cargo run --release -p quarry-egui -- huge.csv
```

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

Generate a deterministic local fixture:

```bash
cargo run --release -p quarry-cli --bin quarry -- generate \
  --size 10GB --columns 40 --delimiter , \
  --output fixtures/generated/test-10gb.csv --seed 1
```

See the [12 GB engine benchmark](docs/benchmarks/2026-08-14-large-file.md),
[egui spike results](docs/benchmarks/2026-08-14-egui-spike.md),
[AppKit spike results](docs/benchmarks/2026-08-14-appkit-spike.md),
[initial engine decision](docs/adr/0001-initial-engine.md),
[viewport cache decision](docs/adr/0002-defer-viewport-cache.md), and
[UI decision](docs/adr/0003-select-egui-ui.md).

Quarry is dual-licensed under MIT or Apache-2.0.
