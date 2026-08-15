# Quarry

> A performance-first, open-source macOS application for exploring and transforming massive delimited text files.

## Mission
Quarry exists for a simple reason: a file should not become unusable just because it is larger than RAM.

The first objective is deliberately narrow: make a 10 GB CSV practical to open and navigate on a Mac without loading the entire file into memory.

## Core promise
**Open huge delimited files quickly, keep the interface responsive, and make common data operations practical.**

Initial capabilities: CSV/TSV/pipe-delimited files, progressive opening, virtualized rows/columns, search, filtering, column operations, split/join transformations, and safe streaming export.

## Performance direction
The initial reference workload is a **10 GB delimited file**. Quarry should show useful first rows within seconds, keep memory bounded, remain interactive during scans, and avoid full-file copies for read-only work.

## Architecture
The data engine will be written in **Rust**. The UI technology is intentionally undecided and will be selected through benchmarked prototypes rather than convention.

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
navigation before a production GUI is selected.

```bash
cargo run --release -p quarry-cli -- open huge.csv
```

Request a row range with `--jump`; Quarry serves it as soon as background
indexing reaches that range, then continues indexing:

```bash
cargo run --release -p quarry-cli -- open huge.csv \
  --jump 100000000 --jump-count 3
```

Generate a deterministic local fixture:

```bash
cargo run --release -p quarry-cli --bin quarry -- generate \
  --size 10GB --columns 40 --delimiter , \
  --output fixtures/generated/test-10gb.csv --seed 1
```

See the [12 GB benchmark](docs/benchmarks/2026-08-14-large-file.md) and
[initial engine decision](docs/adr/0001-initial-engine.md).

Quarry is dual-licensed under MIT or Apache-2.0.
