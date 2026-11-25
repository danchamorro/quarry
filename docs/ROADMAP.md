# Quarry Roadmap

Quarry's live backlog, open issues, and current priorities are tracked on GitHub:

- [Quarry GitHub Project](https://github.com/users/danchamorro/projects/1)
- [Open GitHub Issues](https://github.com/danchamorro/quarry/issues)
- [Changelog](../CHANGELOG.md)
- [v1.0 Claim Contract Ledger](V1_CLAIM_LEDGER.md)

## Product Direction

Quarry is a free, open-source (`MIT OR Apache-2.0`), 100% local-first desktop and CLI editor for multi-gigabyte CSV, TSV, and delimited text files on **macOS** and **Linux / Omarchy**.

The production desktop application is built with Rust and **GPUI** (`apps/quarry-gpui`, [ADR 0013](adr/0013-adopt-gpui-desktop-frontend.md)) on top of the shared headless engine (`crates/quarry-core`, `crates/quarry-delimited`, `crates/quarry-workbench`). The earlier `egui` desktop prototype (`apps/quarry-egui`) was retired after GPUI reached full feature parity and cut over as the shipping application.

## Shipped Highlights (on `main`)

- **Core streaming engine & CLI (`quarry-cli`):** Parallel positional `pread`/`seek_read` I/O (no `mmap` on user source files), 64-byte SIMD structural indexing (Apple Silicon NEON and x86_64 AVX2), privacy-preserving persistent sidecar index cache, and multi-encoding + `.csv.gz` support.
- **Parallel out-of-core transforms:** Adaptive 300 MB in-RAM fast path with automatic bounded disk spilling, persisted scratch directory, multi-threaded stable Sort (1–16 columns including Natural and Date/time), parallel Find & Remove Duplicates (key columns or whole rows), parallel Replace All, and partitioned multi-threaded Hash-Join with sort-merge skew fallback.
- **GPU-accelerated desktop workbench (`quarry-gpui`):** Native macOS and Linux/Omarchy Wayland/Hyprland UI, Light / WCAG AA Dark / System / live Omarchy theme sync, multi-window support for up to 200 open tabs with automatic dormant-tab eviction (`< 128` open FDs), AccessKit (macOS) and AT-SPI (Linux) accessibility, and zero network/telemetry dependencies.
- **Large-file controls, sections & bookmarks:** Pause/Resume Indexing, Live Resource HUD (`phys_footprint` / `VmRSS` and free disk), **Open Section…** (`Bytes`–`TiB`, verified on a 20 TiB sparse file) with outward boundary snapping, `Verified` vs `Speculative` boundary verification, **Save Section As New File**, **Guarded Section Splice-Save** (APFS CoW / Linux `FICLONE` with streaming fallback), and **Session Row Bookmarks**.

## Upcoming Toward v1.0

Upcoming milestones are broken into issues just-in-time on the [GitHub Project](https://github.com/users/danchamorro/projects/1):

1. **Large-file ergonomics & live streaming:** Adaptive word wrap with automatic scale-back on large files, and opt-in append-only **Live Watch Mode** for actively growing files.
2. **Power-user UX & querying:** Interactive `/` regex & expression query bar, keyboard-first navigation (`hjkl`, `gg`, `G`), fuzzy Command Palette (`Cmd/Ctrl+K`), Cell Inspector Drawer (JSON, stack traces, Markdown), Smart Copy (`CSV`, `Markdown`, `JSON`, `SQL IN`), multi-row freeze & column pinning, async column summary headers, Focus Mode, and custom TOML themes.
3. **Splitting, combining & format conversion:** Guided File Splitter (every N rows or at bookmarked rows with header/footer injection), Combine Files v2 (opt-in column alignment, schema-drift flagging, `_source_file` column), `.csv.zst` support, format converters (`CSV`/`TSV`/`PSV`/`JSON`/`NDJSON`/`Markdown`), and reusable custom CSV format profiles.
4. **Analysis & data tools:** Out-of-core Pivot Tables (`SUM`, `AVG`, `COUNT`, `MIN`, `MAX`), Autofill (series/dates) & Flash Fill by example, Combine Lines rollup, Transpose, and Extract Frequent Strings.
5. **Cross-platform packaging & release verification:** Signed/notarized macOS builds (Apple Silicon and Intel), Linux distribution packages, hardware-verified 120 Hz frame pacing and accessibility checks, and full [v1.0 Claim Contract Ledger](V1_CLAIM_LEDGER.md) reconciliation.

## Governance

- Use GitHub Issues and the Project board to track concrete bugs, features, and PR progress.
- Every shipped claim in public release documentation is reconciled against empirical evidence in [`docs/V1_CLAIM_LEDGER.md`](V1_CLAIM_LEDGER.md).
- Keep all source builds and official releases completely free and open source under `MIT OR Apache-2.0` with zero telemetry and zero network calls.
