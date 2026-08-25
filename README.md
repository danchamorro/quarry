<p align="center">
  <img src="assets/quarry-wordmark.png" alt="Quarry" width="480">
</p>

# Quarry

<p align="center">
  <strong>A macOS editor for large CSV and delimited files, validated at 12 GB and 50 GB.</strong>
</p>

Quarry combines a familiar editable data grid with a bounded Rust engine. Open,
inspect, search, filter, edit, reshape, and sort files that conventional
spreadsheet workflows may struggle to load.

The first rows appear without waiting for the complete file scan. Quarry keeps
working memory bounded and does not change the source until you choose
**Save**.

> **Current status:** installable macOS alpha. Core viewing, editing,
> transformation, filtering, sorting, and file-safety workflows are complete.

## Why Quarry

- **Start working quickly.** Progressive opening returns useful rows before the
  structural index reaches the end of the file.
- **Work in a real editor.** Change cells and headers directly, select numbered
  columns, reshape data, sort rows, and keep working before saving.
- **Keep control of the source.** Browsing is read-only, changes remain unsaved
  until you save them, and cancelled operations publish no partial output.

## Capabilities

| Area | What Quarry supports |
|---|---|
| Open and navigate | CSV, TSV, pipe, and semicolon delimiters; progressive first rows; continuous scrolling; direct row jumps; page navigation |
| Work with wide files | Horizontal access to every shown column; persistent one-based column numbers; resize and auto-fit; view-only hide, show, and reorder |
| Find and replace | Literal Find Next, Replace in Cell, and cancellable Replace All across data cells; case-insensitive by default with a per-tool Match case option |
| Filter and export | Right-click a cell to keep or exclude its exact value; Contains, Equals, and Does not equal predicates; same-column alternatives with AND across columns; case-insensitive by default with a per-tool Match case option; incremental results; bounded match indexes; cancellable filtered export |
| Edit | Direct cell and header editing; multiline values; Undo and Redo; Discard Changes |
| Reshape columns | Select columns in the grid, then Split, Combine, Move, or Delete; continue editing the result before saving |
| Sort | Stable ascending and descending text sort; case-insensitive by default with a per-tool Match case option; fixed header; deterministic order for equal values; cancellation and disk preflight |
| Save safely | Atomic Save; no-clobber Save As; source-change detection; cancellation cleanup; no partial published output |

## Install Quarry

Quit any running Quarry copy, then install and verify the application:

```bash
./scripts/macos-app.sh install
./scripts/macos-app.sh verify
open /Applications/Quarry.app
```

The installer verifies the new bundle before replacing the active app and
retains a valid prior build for rollback. See the
[macOS packaging guide](docs/MACOS_PACKAGING.md) for update, rollback, signing,
and package-only instructions.

## Performance at 12 GB and 50 GB

Quarry uses two complementary benchmark tracks that answer different product
questions:

| Track | What it validates | Dataset |
|---|---|---|
| **12 GB feature suite** | Depth across viewing, searching, filtering, exporting, editing, Replace All, Split, Combine, and Sort | Two 12 GB-class fixtures |
| **50 GB scale suite** | The same bounded architecture across every major full-file engine path at a larger scale | Private 48.25 GiB file with 225,437,755 rows |

The 12 GB track uses an 11.33 GiB viewer file with 117,168,829 rows and a
separate deterministic 12,000,000,037-byte write fixture with 61,413,211 rows.

### Opening and navigation

| Measurement | 12 GB suite | 50 GB suite |
|---|---:|---:|
| First 100 parsed rows | 4.828 ms | 3.085 ms |
| Complete structural index | 21.521 s | 72.957 s |
| Index throughput | 539.20 MiB/s | 677.23 MiB/s |
| Random 100-row viewport p50 | 1.699 ms | 1.263 ms |
| Random 100-row viewport p95 | 1.843 ms | 1.570 ms |
| Peak RSS for the viewport run | 14.64 MiB | 5.41 MiB |

### Major full-file operations

| Operation | 12 GB evidence | 50 GB evidence |
|---|---|---|
| Find, absent literal | Complete scan in 44.491 s | Complete scan in 121.591 s |
| Filter, match-retaining | Match-heavy filter: 100,295,554 matches in 42.779 s | Two-rule filter: 5,368,672 matches in 109.659 s |
| Filtered export | Two-match export in 45.511 s | Zero-match export in 119.399 s |
| Sparse-edit Save As | Three edits rewritten in 16.581 s | Two edits rewritten in 79.776 s |
| Replace All | 291,058 replacements in 61.541 s | 1,044,664 replacements in 328.476 s |
| Split | Explicit two-way rewrite in 65.314 s | Automatic analysis and rewrite in 455.508 s |
| Combine | Two-column rewrite in 63.289 s | Two-column rewrite in 328.264 s |
| Stable Sort | 117,168,829 rows by `FIRSTNAME` in 2 min 22.211 s | 225,437,755 rows in 18 min 33.443 s (pre-optimization run) |

Across these full-file operations, the largest reported non-sort peak RSS was
25.62 MiB at 12 GB and 24.53 MiB at 50 GB. The current 12 GB Stable Sort used
49.89 MiB peak RSS and 25.91 GiB peak temporary disk. The earlier 50 GB sort
used 19.38 MiB peak RSS and 102.91 GiB peak temporary disk.

These are verified results at two scales, not a controlled scaling contest.
The fixtures, predicates, cache states, and application revisions differ.
Operation times exclude later benchmark-only validation passes. macOS caches
were not purged, so no result is presented as a controlled cold-cache claim.
The 50 GB sort still proves completion and correctness, but its timing predates
the adaptive merge optimization and is not a current throughput estimate.

<details>
<summary><strong>Benchmark reports and methodology</strong></summary>

- [12 GB progressive open and navigation](docs/benchmarks/2026-08-14-large-file.md)
- [12 GB continuous desktop scrolling](docs/benchmarks/2026-08-15-continuous-scroll.md)
- [12 GB streaming Find](docs/benchmarks/2026-08-15-streaming-search.md)
- [12 GB streaming Filter](docs/benchmarks/2026-08-16-streaming-filter.md)
- [12 GB filtered export](docs/benchmarks/2026-08-16-filtered-export.md)
- [12 GB direct editing and Save As](docs/benchmarks/2026-08-18-direct-cell-editing.md)
- [12 GB Replace All](docs/benchmarks/2026-08-22-12gb-replace-all.md)
- [12 GB Split and Combine](docs/benchmarks/2026-08-19-split-join-transformations.md)
- [Original deterministic stable Sort validation](docs/benchmarks/2026-08-21-stable-text-sort.md)
- [Current 12 GB `FIRSTNAME` sort optimization](docs/benchmarks/2026-08-23-12gb-sort-performance.md)
- [50 GB capability and stress suite](docs/benchmarks/2026-08-22-50gb-capability-suite.md)

</details>

## Safety by default

- Browsing, navigation, Find, Filter, column views, and copy never rewrite the
  source.
- Cell edits and structural changes remain sparse or use private working files
  until an explicit Save or Save As succeeds.
- Save publishes only after a complete same-directory temporary file is
  flushed and synchronized. It also detects metadata-visible source changes.
- Save As refuses to overwrite an existing destination. Cancellation or
  failure removes unpublished temporary artifacts.
- Sort checks a conservative temporary-disk allowance and verifies ordering,
  row preservation, and stable ties before publication.

## How Quarry stays responsive

- The Rust engine scans delimited records incrementally instead of loading the
  file into an in-memory table.
- An adaptive structural index stores sparse checkpoints for direct row access
  without retaining one entry per row.
- The grid materializes only visible rows plus a small overscan and paints a
  bounded horizontal column window while keeping every shown column reachable.
- Find, Filter, export, Replace All, Save, and column transformations use
  cancellable background workers with bounded buffers.
- Stable Sort uses disk-backed runs and guarded publication rather than keeping
  every row in memory.

The production desktop interface uses egui. The data engine remains independent
of the UI framework and is exercised directly by the benchmark CLI.

## Using Quarry

1. Open a delimited file from Finder, **Choose…**, drag and drop, or a typed
   path. Select a delimiter or header override only when automatic detection is
   not right for the file.
2. Scroll continuously, jump to a one-based data row, page through the file, or
   horizontally scroll through every shown column.
3. Double-click a cell or header to edit it. Find/Replace ignores case by
   default; turn on its **Match case** option for exact matching. Right-click a
   data cell to Copy, Filter to This Value, or Filter Out This Value. Use
   Filters to build literal rules and export the matches. The Filters tool also
   ignores case by default and has its own **Match case** option; the two
   right-click filter actions inherit that Filters setting. Equals and Contains
   values in one column are alternatives; every filtered column must match.
4. Select numbered column headers, then right-click to Split, Combine, Move,
   Delete, or Sort. Sort ignores case by default and has its own **Match case**
   option. Completed operations return to the ordinary editable grid.
5. Use Undo, Redo, or Discard Changes while experimenting. Choose Save to update
   the guarded source or Save As to publish a separate file.

## Development

Launch the desktop app directly from Cargo:

```bash
cargo run --release -p quarry-egui -- huge.csv
```

Run the benchmark-oriented CLI:

```bash
cargo run --release -p quarry-cli --bin quarry-bench -- open huge.csv \
  --rows 100 --cache-state unknown
```

Generate a deterministic local fixture:

```bash
cargo run --release -p quarry-cli --bin quarry -- generate \
  --size 10GB --columns 40 --delimiter , \
  --output fixtures/generated/test-10gb.csv --seed 1
```

The benchmark reports contain the complete reproduction commands, validation
rules, cache declarations, resource measurements, and limitations.

## Project documents

- [Product requirements](docs/PRD.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Roadmap](docs/ROADMAP.md)
- [Engineering principles](docs/ENGINEERING_PRINCIPLES.md)
- [Benchmark archive](docs/benchmarks/)
- [Contributing](docs/CONTRIBUTING.md)

## Built with AI, in the open

Quarry is built with AI coding agents under human product direction. Agents
implement, benchmark, test, document, and iterate. The human owner sets the
product vision, evaluates the experience, and accepts the decisions. Source,
architecture decisions, performance evidence, tests, and commit history remain
public so the process and results can be inspected.

## License

Quarry is dual-licensed under MIT or Apache-2.0.
