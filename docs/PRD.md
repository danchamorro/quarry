# Quarry Product Requirements Document

**Status:** Living product requirements (targeting `v1.0`, current beta series `v0.2.0`)
**Platforms:** macOS and Linux / Omarchy (native Wayland / Hyprland)
**Architecture:** Shared Rust streaming engine (`crates/quarry-delimited`, `crates/quarry-core`, `crates/quarry-workbench`) with a GPU-accelerated **GPUI** desktop frontend (`apps/quarry-gpui`, [ADR 0013](adr/0013-adopt-gpui-desktop-frontend.md)) and headless CLI (`apps/quarry-cli`)
**License & Privacy:** Free and open source (`MIT OR Apache-2.0`), 100% local-first, zero telemetry, zero network calls

Current delivery progress, upcoming milestones, and quantitative claim verification live in [`docs/ROADMAP.md`](ROADMAP.md), [`docs/V1_CLAIM_LEDGER.md`](V1_CLAIM_LEDGER.md), and [GitHub Issues](https://github.com/danchamorro/quarry/issues).

---

## Product Vision

Quarry is a performance-first, local-only desktop and CLI editor for people who inspect, query, clean, reshape, and edit delimited text files far too large for spreadsheets and conventional text editors.

> **North star:** Open a 10+ GB CSV on macOS or Linux, see useful rows in milliseconds, and navigate, filter, sort, join, and edit it with bounded memory—or slice into multi-terabyte files via bounded section windows—without ever risking silent corruption of the source file.

Quarry is completely free and open source under `MIT OR Apache-2.0`. Every feature is available in source builds and official releases with no paid tiers, license keys, telemetry, or Contributor License Agreement (see the [project policy](../README.md#free-open-source-and-local-first)).

### Target Users

Data engineers, data operations teams, ETL and integration developers, analysts, database engineers, and technical users who regularly inspect and repair multi-gigabyte CSV, TSV, PSV, and semicolon-delimited exports.

### Product Principles

1. **Performance is a feature.** Multi-gigabyte files are the normal case, not an edge case.
2. **File size never dictates RAM usage.** Every operation uses bounded memory and explicit out-of-core disk spilling when needed.
3. **Useful rows appear immediately.** Progressive opening renders the first viewport before background indexing finishes.
4. **Never freeze the interface.** Long operations run off the UI thread with progress reporting and prompt cancellation.
5. **Correctness and data safety outrank cleverness.** Never mutate source bytes in place without guarded atomic staging and source-identity verification; fail explicitly rather than silently corrupting data.
6. **Thin UI over a shared Rust engine.** `quarry-core` and `quarry-workbench` own data processing, document lifecycles, and safety invariants so desktop and CLI share identical semantics.
7. **Evidence over guesswork.** Quantitative claims are tied to reproducible benchmarks and verified in [`docs/V1_CLAIM_LEDGER.md`](V1_CLAIM_LEDGER.md).
8. **Design for Quarry's own identity.** Solve large-file data workflows cleanly without blindly chasing feature parity with unrelated editors.

---

## Core Requirements (Implemented on `main`)

### 1. Streaming Engine, Indexing & Scale

- **Progressive open & lazy decoding:** Sample a bounded prefix to detect delimiter/header/BOM and render the first viewport immediately while background indexing builds byte-stride structural checkpoints under a bounded memory budget.
- **Parallel positional I/O (no `mmap` on user source files):** Read source files via positional `pread` / `seek_read` across an engine-owned bounded worker pool so external truncation or media removal surfaces as a clean error instead of `SIGBUS` ([ADR 0001](adr/0001-initial-engine.md)).
- **SIMD structural scanning:** Runtime-dispatched 64-byte SIMD block classification (Apple Silicon NEON, x86_64 AVX2, and pure-scalar fallback) verified against a scalar reference oracle for RFC 4180 quotes, escaped quotes, embedded newlines, CRLF/LF, and malformed-record boundaries.
- **Privacy-preserving persistent sidecar index cache:** Optional local cache under owner-only permissions (`0700`/`0600`) with keyed opaque filenames storing only structural offsets, file identity, and content hashes—never source filenames, headers, or cell values—with automatic opt-out on network/FAT/exFAT volumes and a user-accessible clear/disable control.
- **Encodings & compression:** Explicit support for Auto (Unicode BOM, otherwise UTF-8), UTF-8, UTF-16 LE/BE, Windows-1252, and ISO-8859-1 with no silent legacy guessing; read-only streaming `.csv.gz` support via private temporary decompression ([ADR 0012](adr/0012-compressed-delimited-files.md)).

### 2. Desktop Workbench, Themes & Accessibility

- **Native GPU-rendered desktop UI (`apps/quarry-gpui`):** Cross-platform desktop application for macOS and Linux/Omarchy (native Wayland / Hyprland, fractional scaling, native macOS menu bar and Linux in-window menu bar, XDG portals).
- **Theme system & typography:** Built-in **System**, **Light**, WCAG AA **Dark**, and hardened live **Omarchy** theme sync backed by a single semantic token palette ([Design System](DESIGN.md)); OpenType `tnum` tabular figures with automatic numeric right-alignment and configurable inline whitespace markers.
- **Multi-window, 200-tab scalability:** Single-process workbench supporting multiple windows and up to 200 open tabs with automatic clean-tab dormancy and a strict `< 128` open file descriptor budget ([ADR 0007](adr/0007-document-tabs.md)).
- **Accessibility:** Full semantic trees via `accesskit_macos` (macOS) and `accesskit_unix` / AT-SPI (Linux), keyboard operability across controls and dialogs, and quiet status telemetry that announces real state transitions without flooding screen readers.

### 3. Large-File Controls, Sections, Bookmarks & Live Watch

- **Pause / Resume Indexing & Live Resource HUD:** Pause and resume background indexing without parking worker pool threads, paired with a live HUD displaying process memory (`phys_footprint` / `VmRSS`), per-volume free disk space, and active tab counts.
- **Bounded Section Windows (`Open Section…`):** Open arbitrary byte ranges (`Bytes`–`TiB`, verified on a 20 TiB sparse file) with outward line-boundary snapping, `Verified` vs `Speculative` boundary trust status, pausable **Verify Section Boundary**, **Save Section As New File**, and `Verified`-only **Guarded Section Splice-Save** (APFS CoW / Linux `FICLONE` with streaming fallback) ([ADR 0014](adr/0014-large-file-controller-and-section-windows.md), [ADR 0015](adr/0015-guarded-section-splice-save.md)).
- **Session Row Bookmarks:** Up to 10,000 session-only bookmarks per document with `#` gutter markers, toolbar popover, jump shortcuts (`Cmd/Ctrl+B`, `Cmd/Ctrl+]`, `Cmd/Ctrl+[`), and preservation across edits, inserts/deletes, filters, and tab dormancy.
- **Adaptive Word Wrap:** Opt-in two-line cell wrapping (`Option+Z` / `Alt+Z`) with bounded layout caching and a persisted auto-disable file-size threshold (default `300 MB`, with one-click per-tab override).
- **Live Watch Mode:** Opt-in append-only tailing (`Cmd/Ctrl+Shift+L`) using native `kqueue` (macOS), `inotify` (Linux), or bounded network polling, with provisional tail handling, scroll-preserving follow, incremental updates for active filters, and rotation/truncation recovery ([ADR 0016](adr/0016-live-watch-mode.md)).

### 4. Search, Filtering & Querying

- **Overlay-aware Search & Replace:** Parallel literal **Find**, **Replace in Cell**, and bounded parallel **Replace All** honoring unsaved sparse cell edits and per-tool **Match case** settings.
- **Multi-column Filters & Filtered Export:** Compound filters combining same-column alternatives, exclusions (`Does not equal`), text operators (`Contains`, `Equals`), and exact arbitrary-precision decimal numeric rules (`>`, `>=`, `<`, `<=`, `Between`) without floating-point rounding. Filters include unsaved sparse cell edits, refresh asynchronously on value edits/Undo, and stream cleanly to **Export Filtered…**.
- **Bounded Regex & Filter Expressions:** Opt-in ReDoS-bounded regular expressions (`quarry search --regex`, `filter --operator regex`, `export --operator regex`, and Live Watch filter evaluation) with explicit pattern/DFA/nesting limits, plus an exact `FilterQuery` expression parser/printer (`--where EXPR` in `quarry-cli`) ready for desktop query-bar integration.

### 5. Direct Editing, Safety Guards & Recovery

- **Sparse overlays & bounded history:** Direct cell and header editing (including multiline UTF-8 values) stored as a sparse overlay keyed by physical record identity; bounded individual Undo/Redo (1,000 entries / 16 MiB payload) integrated with one-level structural working-copy Undo/Redo.
- **Atomic guarded writes:** Streaming **Save**, no-clobber **Save As**, and **Save As UTF-8** using owner-only (`0700`/`0600`) same-volume staging, flush + sync, permission/ACL preservation, and source-stamp conflict checks before replacement. Final-path symlinks require Save As.
- **Temporary storage & preflight:** Persisted scratch folder choice (revalidated on every use; on Linux automatically avoiding RAM-backed `tmpfs` when unset), upfront capacity checks, and an explicit storage-review prompt for allowances `>= 256 MiB` ([ADR 0005](adr/0005-temporary-storage.md)).
- **Inspect File & Recovery:** Cancellable diagnostic scan surfacing malformed quotes, encoding errors, oversized records, and ragged widths, with selective **Save Recovery Copy…** that leaves the source untouched ([Inspection validation](INSPECTION_VALIDATION.md)).

### 6. Out-of-Core Transforms & Multi-File Workflows

- **Adaptive 300 MB fast path:** Out-of-core Sort, Duplicates, and Replace All draw from a shared memory budget (default 300 MB) to complete small/medium files in memory without disk spills while retaining identical external-merge semantics above the threshold.
- **Multi-column Sort:** Stable parallel external sort across 1–16 columns supporting Text (case-insensitive/sensitive), exact arbitrary-precision Number, Natural (`v1.9 < v1.10`), explicit Date / time ([ADR 0008](adr/0008-date-time-sorting.md)), Character count, Word count, Shuffle, and Reverse ([ADR 0010](adr/0010-multi-column-sorting.md)).
- **Find & Remove Duplicates:** Parallel bounded external deduplication over selected columns or whole rows, previewing exact extra-row counts before removal and keeping the first occurrence in current order ([ADR 0004](adr/0004-bounded-duplicate-removal.md)).
- **Structural & Cleanup Operations:** Selected-column **Clean Up** (Trim, Collapse spaces, Uppercase, Lowercase, Fill empty), **Split**, **Combine**, **Move**, **Delete**, and **Insert Rows / Columns** ([ADR 0009](adr/0009-row-column-insertion.md)), plus view-only column search/hide/reorder/auto-fit.
- **Combine Files & Join Files:** Multi-file exact-schema streaming **Combine Files** ([ADR 0006](adr/0006-combine-files.md)) and 4-mode (Inner, Left, Right, Full) compound-key **Join Files** using partitioned multi-threaded hash-join with automatic external sort-merge skew fallback ([ADR 0011](adr/0011-bounded-file-joining.md)).

---

## Remaining Requirements for `v1.0`

Tracked at milestone level in [`docs/ROADMAP.md`](ROADMAP.md) and contract level in [`docs/V1_CLAIM_LEDGER.md`](V1_CLAIM_LEDGER.md):

1. **Interactive Exploration & Keyboard-First UX:** Inline `/` desktop Query Bar (wiring the core regex and expression parser), Vim/arrow cell cursor (`hjkl`, `gg`, `G`), fuzzy Command Palette (`Cmd/Ctrl+K`), syntax-highlighted Cell Inspector Drawer, rectangular Smart Copy (`CSV`, `Markdown`, `JSON`, `SQL IN`), multi-row freeze & column pinning, async column summary headers, Focus Mode, and custom TOML themes.
2. **File Workflows, Dialects & Interchange:** Guided File Splitter (every N rows or at bookmarks, with optional header/footer injection), Combine Files v2 (opt-in column alignment, schema-drift report, `_source_file`), `.csv.zst` compression, format converters (`CSV`/`TSV`/`PSV`/`JSON`/`NDJSON`/`Markdown`), and reusable custom CSV format profiles.
3. **Built-In Analysis & Shaping Tools:** Out-of-core Pivot Tables (`SUM`, `AVG`, `COUNT`, `MIN`, `MAX`), Autofill (series/dates) & Flash Fill by example, Combine Lines rollup, Transpose, and Extract Frequent Strings.
4. **Release & Platform Verification:** Signed/notarized Apple Silicon and Intel macOS releases, Linux distribution packages, hardware-verified 120 Hz frame pacing and accessibility checks, and full `V1_CLAIM_LEDGER.md` reconciliation.

### Explicit Non-Goals (for `v1.0`)

General-purpose code/text editing, spreadsheet formula recalculation engines, remote database connectors, cloud sync, real-time collaboration, telemetry/analytics, paid feature gating, and silent in-place byte mutation.

---

## Performance, Packaging & Reliability Bar

- **Measured benchmarks:** Every performance claim must record hardware, OS, dataset SHA-256, build mode, thread count, and cache state (see [`docs/BENCHMARK_DATASETS.md`](BENCHMARK_DATASETS.md) and [`docs/benchmarks/`](benchmarks/)).
- **Packaging & integrity:**
  - **macOS:** Canonical `/Applications/Quarry.app` bundle (`io.github.danchamorro.quarry`), Developer ID signed, hardened runtime, notarized, stapled, and shipped as `.dmg` and `.zip` with `SHA256SUMS` and verified local rollback ([macOS Packaging](MACOS_PACKAGING.md)).
  - **Linux & Omarchy:** Architecture-specific `x86_64` and `arm64` tarballs with XDG `.desktop` integration, manifest-tracked `install.sh` / `uninstall.sh`, and audited third-party notices ([Linux & Omarchy Guide](LINUX_AND_OMARCHY.md)).
- **Zero-network verification:** Release dependency graphs, linked binaries, and offline runtime socket checks verify zero HTTP/TLS/network transport dependencies across shipping targets.

---

## Positioning

> **Quarry — the free, open-source, GPU-accelerated editor for massive CSV and delimited files on macOS and Linux.**

Another editor's trademark must never be used as Quarry's official tagline.
