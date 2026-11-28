# Changelog

All notable changes to Quarry are documented here. This file is maintained by
hand. The format is based on [Keep a Changelog](https://keepachangelog.com/).

Changes are grouped by the day they merged into `main` (US Eastern time).
Tagged beta releases are noted on the day they were published; their GitHub
Release bodies are generated from this file. Versioned notes under
`docs/releases/` are reserved for major releases. Numbers in parentheses are
GitHub pull requests.

## [2026-11-27]

### Added

- Added an inline **Query Bar** to the desktop app via **View → Query Bar**, `Cmd+Shift+F` (macOS) / `Ctrl+Shift+F` (Linux), or the toolbar **Query** button so users can filter tables directly with SQL-like `AND` / `OR` expressions, ranges, and regular expressions. (#238)
- Added two-way synchronization between the **Query Bar** and the **Filters** popover (including a new **Matches regex** operator), per-tab query drafts, inline diagnostics that leave active filters untouched on syntax errors, and an opt-in **Live** as-you-type toggle that defaults to Enter-to-apply on files larger than 10 MiB. Documented in `docs/USER_GUIDE.md`. (#238)
- Added `--where EXPR` (along with `--delimiter` and `--header`) to `quarry filter` and `quarry export`, letting users write concise multi-column filter expressions using column names, `[bracketed]` headers, or `#N` column indices instead of repeated `--column`/`--and` flags. (#234)
- Added an exact, bounded filter expression parser and canonical printer supporting `AND` across columns, same-column `OR`, `!=` exclusions, numeric comparisons, `BETWEEN` / inclusive range folding, `CONTAINS`, and `=~` / `/pattern/` regex rules, with explicit diagnostics for unsupported syntax or ambiguous duplicate headers and early validation before gzip decompression. Documented in `docs/USER_GUIDE.md`. (#234)
- Added opt-in bounded regular expression matching to the CLI (`quarry search --regex`, `quarry filter --operator regex`, and `quarry export --operator regex`, plus `--ignore-case`) and core search, filter, filtered export, and live-watch engines while keeping literal matching as the default. (#232)
- Added explicit regex validation limits (4 KiB pattern length, 2 MiB compiled and DFA budgets, syntax nesting depth of 64, and up to 16 regex rules per compound filter) and documented Unicode simple case folding vs literal ASCII case folding in `docs/USER_GUIDE.md`. (#232)

### Changed

- Sped up GitHub Actions CI by adding Rust dependency caching across all jobs, removing the duplicate release compile of the desktop app in the macOS packaging check, aligning test and lint feature flags, and adding a fail-closed fast path for documentation-only pull requests while keeping full LTO release builds for tagged releases. (#236)
- Retired the unused GitHub Project board links across `README.md`, `docs/ROADMAP.md`, `docs/README.md`, `docs/ROADMAP_HISTORY.md`, and the issue template, pointing contributors directly to `docs/ROADMAP.md` and GitHub Issues. (#235)
- Replaced the outdated `v0.1`–`v0.5` log in `docs/PRD.md` with a current product requirements document and corrected stale temporary-storage, indexing, sorting, and tab-dormancy details in `docs/ARCHITECTURE.md`, `docs/USER_GUIDE.md`, and `README.md`. (#235)
- Refreshed audited third-party license notice inventories across macOS Apple Silicon and Linux `x86_64` / `arm64` after adding `regex` to `quarry-core` (with no new third-party crates added to `Cargo.lock`). (#232)
- Updated **Download & Install** links and asset filenames in `README.md` and `docs/LINUX_AND_OMARCHY.md` to point to the published `v0.2.0-beta.3` release. (#230)

### Fixed

- Corrected the documented `v0.2.0-beta.3` clean Git revision in `docs/BETA_TESTER_GUIDE.md`, `docs/BETA_RELEASE_CHECKLIST.md`, and `docs/MACOS_PACKAGING.md` from the annotated tag object (`d57dec9`) to the tagged commit (`c3febf3`) embedded in release bundles. (#235)

## [2026-11-26]

### Added

- Added `quarry --version`, `-V`, and `version` to print the CLI version. (#229)
- Added opt-in append-only **Live Watch Mode** via **Data → Live Watch** (`Cmd+Shift+L` on macOS / `Ctrl+Shift+L` on Linux) and the toolbar **Watch** button to stream newly appended rows live from active CSV/TSV writers using native `kqueue` (macOS), `inotify` (Linux), or bounded polling on network volumes. (#228)
- Added provisional tail handling, scroll-preserving tail follow (with **Follow Tail** and **Stop Watch** controls), incremental updates for active filters while watching, and automatic alerts offering **Open New File** on rename rotation or **Reload from Disk** on truncation or tail rewrites. Documented in [ADR 0016](docs/adr/0016-live-watch-mode.md). (#228)
- Added Linux and Omarchy desktop packaging (`scripts/linux-app.sh`, `packaging/linux/`) with a Wayland `.desktop` launcher, app icon, manifest-tracked `install.sh` / `uninstall.sh`, and automated `x86_64` and `arm64` tarball builds in `.github/workflows/release.yml`. (#224)
- Added audited third-party license notice inventories for Linux `x86_64` and `arm64` (covering both the desktop app and `quarry` CLI) alongside macOS Apple Silicon in `packaging/licenses/`. (#224)
- Added `./scripts/macos-app.sh release` to build, Developer ID sign, Apple-notarize, staple, and checksum Apple Silicon `.dmg` and `.zip` release artifacts using either the local Keychain profile or CI credentials. (#223)
- Added a tag-triggered GitHub Actions release workflow (`.github/workflows/release.yml`) and changelog extractor (`scripts/extract-release-notes.py`) that create Draft GitHub Releases directly from `CHANGELOG.md`. (#223)
- Added opt-in adaptive word wrap for grid cells via **View → Word Wrap**, `Option+Z` (macOS) / `Alt+Z` (Linux), and the toolbar **Wrap** toggle, displaying up to two wrapped lines per row while preserving top-row scroll position and bounded frame budgets. (#221)
- Added a configurable word-wrap auto-disable file-size threshold (default `300 MB`, with one-click per-tab override) under **File → Temporary Storage…** that persists across restarts. (#221)

### Changed

- Updated Linux default temporary scratch storage so that when `TMPDIR` is unset and `/tmp` (or a CLI output directory) is mounted on RAM-backed `tmpfs` or `ramfs`, Quarry automatically uses a private (`0700`) disk-backed `$XDG_CACHE_HOME/quarry/tmp` (defaulting to `~/.cache/quarry/tmp`) while keeping final output staging beside its destination. (#229)
- Updated CLI `--memory-budget` to accept human-readable size suffixes (`512MB`, `512MiB`, `1.5GB`) with exact integer scaling and clearer validation errors, and clarified `sort-save-as` help and missing-argument errors for `--column` and `--order` versus `--mode shuffle|reverse`. (#229)
- Moved **Download & Install** to the top of `README.md` with direct links to the published `v0.2.0-beta.2` macOS and Linux/Omarchy release assets and architecture-parameterized verification commands. (#225)
- Updated `README.md` and `docs/LINUX_AND_OMARCHY.md` with prominent macOS `.dmg` and Linux/Omarchy tarball installation instructions and replaced the outdated `v0.1.0-beta.3` snapshot note. (#224)
- Bumped the Quarry workspace version from `0.1.0` to `0.2.0` for the GPUI beta series and refreshed third-party license notices accordingly (zero third-party dependency changes). (#223)
- Scoped GitHub Actions `push` triggers to `main` and enabled `cancel-in-progress` workflow concurrency for pull requests so superseded commits no longer exhaust macOS runner slots. (#222)

### Fixed

- Fixed desktop idle wake-ups by replacing the permanent 32 ms background polling timer with event-driven wake-ups (parking with zero timers when idle and slowing idle Live Resource HUD sampling to 2 seconds) and switching Linux Omarchy live theme sync to kernel `inotify` with swap-gap protection so theme switches repaint immediately without flashing a fallback palette. (#229)

## [2026-11-25]

### Added

- Added right-click and `Shift+F10` context menus across column headers, data cells (with instant **Filter to This Value** and **Filter Out This Value**), and `#` row gutters (with **Add/Remove Bookmark**, row insert/delete, and **Copy Row**). (#218)
- Added external file drag-and-drop onto the Quarry window, double-clicking a column header to rename it, status-bar **Cancel** buttons for active Save and Indexing jobs, and visible row-range and selection counts in the status bar. (#218)
- Added session-only row bookmarks via **Edit → Toggle Bookmark** (`Cmd+B` / `Ctrl+B`), **Next Bookmark** (`Cmd+]` / `Ctrl+]`), **Previous Bookmark** (`Cmd+[` / `Ctrl+[`), **Clear Bookmarks**, right-clicking the `#` row gutter, and the toolbar **Bookmarks** popover (**View → Bookmarks…**). Bookmarks survive cell edits, filters, saves, and tab dormancy, shift across row insertions and deletions, and restore via **Undo** if cleared by sort or duplicate removal. (#215)
- Added in-place guarded **Save** (`Cmd+S` / `Ctrl+S`) for verified sections, splicing edited section bytes back into the parent file using copy-on-write cloning on APFS and supported Linux filesystems (with streaming fallback elsewhere), advisory disk-space checks, and automatic parent reload notices in open sibling tabs. Documented in [ADR 0015](docs/adr/0015-guarded-section-splice-save.md). (#213)
- Added section-scoped **Insert Rows…**, **Delete Selected Rows**, **Replace All**, and **Undo** / **Redo** for verified sections while keeping header edits, column schema changes, **Sort**, **Find Duplicates**, and **Join Files** routed to **Save Section As New…**. (#213)
- Added a dedicated [Linux & Omarchy Guide](docs/LINUX_AND_OMARCHY.md) and fresh macOS Light, macOS Dark, and Omarchy native Wayland screenshots under `docs/images/`. (#217)

### Changed

- Refreshed `README.md`, the User Guide, Design System, Architecture, Roadmap, Contributing, and macOS Packaging documentation to reflect the GPUI desktop app on macOS and Linux/Omarchy, Light/Dark/System/Omarchy themes, 200-tab multi-window workspaces, section windows, row bookmarks, verified multi-core benchmarks, and free open-source (`MIT OR Apache-2.0`, zero-telemetry) terms. (#217)

### Fixed

- Fixed multi-second click-to-paint stalls on Apple Silicon macOS virtual machines by temporarily patching the macOS GPUI backend to fall back to `CVDisplayLink` on Apple Paravirtual GPUs while keeping bare-metal Macs unchanged, and refreshed the third-party license manifest for the pinned patch. (#219)
- Fixed `Cmd+C` / `Ctrl+C` row copying after selecting a row in the `#` gutter (including while filtered) and fixed **Edit → Rename Selected Column Header…** to target the selected column header rather than the previously focused cell. (#218)
- Fixed grid whitespace visualization to inspect raw unescaped cell bytes, highlight leading, trailing, repeated, tab, newline, and special whitespace with corner indicators and tooltips, distinguish missing ragged fields, and persist **Show All Whitespace** across restarts. (#218)
- Fixed error banners for damaged or unsupported `.gz` files so they no longer offer an unusable **Inspect…** action. (#218)
- Fixed broken cross-file documentation links and heading anchors across `docs/`, including the `ADR 0014` reference to `ADR 0005`. (#217)
- Fixed modal dialog confirm and cancel buttons (**Sort Rows**, **Clean Selected Columns**, **Split Column**, **Open Section…**, and other dialogs) so clicking the primary action or **Cancel** works on the first click even when a dropdown or text input still holds focus. (#216)
- Fixed active grid cell and list selection highlights so selected cell text remains clearly readable in both Light and Dark themes instead of being obscured by an opaque selection fill. (#216)

## [2026-11-24]

### Added

- Added **File → Open Section…** (`Cmd+Shift+O` / `Ctrl+Shift+O`) to open a bounded byte range of a multi-terabyte file without indexing the whole file, with exact `Bytes`–`TiB` bounds, coarse slider presets, and **Save Section As New…** export. Sections must be saved as a new file; in-place **Save** is disabled. (#209)
- Added **Data → Verify Section Boundary** and a `Verified`/`Speculative` banner so mid-file sections can be verified with pause, resume, and cancel controls while preserving unsaved edits if the row boundary shifts. Documented in [ADR 0014](docs/adr/0014-large-file-controller-and-section-windows.md). (#209)
- Added **Data → Pause / Resume Indexing** and an inline status-bar **Pause** / **Resume** control so large-file indexing can pause mid-scan without holding worker threads or file handles, while keeping Find and Filter usable up to the indexed row. (#206)
- Added a per-window Live Resource HUD in the status bar showing process memory and available disk space on the document and temporary-storage volumes, with accessible status announcements on state changes. (#206)

### Changed

- Rewrote recent changelog entries (`2026-11-21` through `2026-11-24`) into a concise, plain-English style matching earlier releases. (#210)

### Removed

- Retired the legacy egui desktop frontend (`apps/quarry-egui`) and its unused `eframe`/`egui_extras` dependencies after the GPUI cutover, leaving the GPUI app as Quarry's sole desktop frontend. Refreshed third-party license manifests and architecture docs while keeping upgrade cleanup for older installs in `scripts/macos-app.sh`. (#207)

### Fixed

- Preserved exact requested section end boundaries in **Open Section…** when a single record longer than 1 MiB ends at the requested offset, while still rejecting unvalidated or mid-quote boundaries and safely clamping speculative scan distances on 32-bit targets. (#211)

## [2026-11-23]

### Added

- Added a verified `rollback [archive-path]` subcommand to `scripts/macos-app.sh` to restore backed-up macOS app bundles with in-process rollback recovery, and recorded the GPUI cutover verification report in `docs/benchmarks/2026-11-23-phase4b3-gpui-cutover-verification.md`. (#204)
- Added macOS desktop integration to the GPUI app, including install-lock coordination with `scripts/macos-app.sh`, Finder file-open and hard-link window routing, and bundled build metadata in **About Quarry**. (#203)
- Added **View → Theme** with **System**, **Light**, **Dark** (WCAG AA), and local **Omarchy** theme sync, plus right-aligned tabular numerals for numeric columns in the GPUI grid. (#200)
- Recorded 200-tab multi-window and accessibility verification on macOS and Linux Wayland in `docs/benchmarks/2026-11-23-phase4b2-gpui-workbench-verification.md`. (#200)
- Added **Combine Files…**, **Join Files…**, **Inspect File & Recovery…**, **Temporary Storage…**, **About Quarry**, and `.gz` decompression/preview banners to the GPUI desktop app, with persistent temporary-storage and index-cache preferences. (#199)
- Added the **Data** menu, inline **Find & Replace** bar, **Export Filtered…**, and dialogs for **Sort Rows…**, **Find & Remove Duplicates…**, **Clean Selected Columns…**, row/column insert and delete, **Move Selected Columns…**, **Split Selected Column…**, and **Combine Selected Columns…** to the GPUI desktop app. (#198)
- Connected the GPUI desktop app to the shared multi-window workbench supporting up to 200 tabs, native macOS and Linux menus, encoding selection, **Save** / **Save As…** / **Save As UTF-8…**, whitespace indicators, and cross-window unsaved-changes prompts. (#197)
- Adopted GPUI as Quarry's v1.0 desktop frontend in [ADR 0013](docs/adr/0013-adopt-gpui-desktop-frontend.md) (superseding [ADR 0003](docs/adr/0003-select-egui-ui.md)) and recorded macOS and Linux Wayland hardening checks in `docs/benchmarks/2026-11-23-phase4b1-gpui-hardening.md`. (#192)
- Added screen-reader accessibility labels, roles, and dialog focus restoration across the GPUI grid, toolbar, banners, and status bar for macOS VoiceOver and Linux AT-SPI. (#192)

### Changed

- Switched macOS `.app` packaging in `scripts/macos-app.sh` and third-party license notices (`packaging/licenses/`) from egui to GPUI, adding five pre-audited OSI licenses and regenerating notices for 397 locked packages. (#204)
- Integrated `apps/quarry-gpui` into the root Cargo workspace with platform-native macOS/Linux shortcuts and zero HTTP/TLS dependencies, upgraded the workspace MSRV and pinned toolchain to Rust 1.100.0, and refreshed third-party license notices. (#191)

### Fixed

- Fixed Shift+scroll horizontal navigation and single-click switching between open toolbar popovers (such as **Format** and **Columns**) in the GPUI desktop app. (#192)

## [2026-11-22]

### Added

- Added a dormant-tab scheduler supporting up to 200 open files under a bounded file-descriptor budget, unloading idle clean tabs while preserving unsaved edits, Undo/Redo history, `.gz` working copies, and external-change checks. Amended [ADR 0007](docs/adr/0007-document-tabs.md). (#187)
- Added UI-independent window, document, and menu-command routing in `quarry-workbench` (starting with dirty-tab close and **Keep Editing** / **Discard**), and documented the frozen status of `apps/quarry-appkit` in `docs/ARCHITECTURE.md`. (#186)
- Recorded parallel out-of-core, adaptive 300 MB memory budget, and hash-join verification benchmarks in `docs/benchmarks/2026-11-22-phase3-out-of-core-benchmarks.md`, confirming golden output parity and up to 60× faster Replace All, 5.5× faster Sort, and 4.7× faster duplicate removal. (#178)
- Added a multi-threaded partitioned hash-join engine to **Join Files** with automatic external sort-merge fallback for heavily skewed keys, cutting 10M-row and 18 GiB join times while staying within bounded memory. Amended [ADR 0011](docs/adr/0011-bounded-file-joining.md) and updated `docs/benchmarks/2026-11-22-phase3c-join-profiling.md`. (#177)
- Added **Natural** alphanumeric sorting (`--mode natural` in `sort-save-as`), whole-row duplicate detection (`duplicates --all-fields` and **All fields (whole row)** in **Find Duplicates**), and bounded date/time format suggestions (`quarry suggest-date-format` and **Sort Rows** hints). Amended [ADR 0004](docs/adr/0004-bounded-duplicate-removal.md) and [ADR 0010](docs/adr/0010-multi-column-sorting.md). (#175)
- Added multi-threaded **Find & Remove Duplicates** and **Replace All** with zero-spill fast paths for smaller files, immediate output index reuse in the desktop app, `--threads` on both CLI commands, and `--memory-budget` on `duplicates`. (#174)
- Added multi-threaded external sorting with an adaptive 300 MB in-memory fast path, `--threads` and `--memory-budget` flags on `sort-save-as`, and persistent custom scratch folders via **File → Temporary Storage…**. Updated [ADR 0005](docs/adr/0005-temporary-storage.md). (#173)

### Changed

- Moved Quarry's Light color palette and shared typography constants into frontend-neutral `quarry-workbench` so desktop frontends share a single theme definition. (#188)
- Separated UI-neutral tab state, delimiter settings, and filter-rule drafts into `quarry-workbench`, keeping widget and grid state local to the desktop frontend. (#185)
- Extracted Quarry's UI-independent document model, edit history, and selection state into a new `quarry-workbench` workspace crate, added it to cross-platform CI jobs, and refreshed third-party license notices. (#184)
- Optimized **Join Files** to store compact row locators instead of full record payloads during sorting and read output rows lazily, reducing 18 GiB peak temporary disk usage by 4× and 10M-row peak memory by 15×. Recorded profiling results in `docs/benchmarks/2026-11-22-phase3c-join-profiling.md`. (#176)

## [2026-11-21]

### Added

- Recorded SIMD and parallel indexing benchmarks in `docs/benchmarks/2026-11-21-phase2-engine-benchmarks.md`, verifying golden output parity across all 30 public-dataset workloads with up to 29 GiB/s warm indexing and sub-21 ms cached reopen times. (#166)
- Added a persistent, owner-private structural index cache (storing structural metadata only, never filenames or cell contents) with a 256 MiB LRU limit, integrity validation, **File → Temporary Storage…** cache controls, and CLI `--no-index-cache`, `--index-cache-dir`, and `clear-index-cache` options. (#165)
- Added multi-threaded literal Find, Filter, and backpressure-bounded Filtered Export across the core engine, with `--threads` support and worker telemetry in `quarry-cli`. (#164)
- Added chunk-parallel structural indexing using positional file reads and a bounded worker pool (no new third-party dependencies), with `--threads` support in `quarry-cli`. Amended [ADR 0001](docs/adr/0001-initial-engine.md) to prefer positional reads over `mmap` on user files. (#163)
- Added runtime-dispatched 64-byte SIMD structural scanning (NEON and AVX2, with scalar fallback and UTF-16 safety) plus configurable delimiter, quote, and escape rules in `quarry-delimited`. (#162)
- Recorded the public-dataset benchmark baseline and golden output digests across all 30 Weather and Hacker News workloads in `docs/benchmarks/2026-11-21-phase1-public-datasets-baseline.md` and `docs/benchmarks/2026-11-21-phase1-public-datasets-golden.json`. (#156)
- Added a pure-scalar reference CSV parser oracle, a seeded torture-corpus generator (`scripts/generate-torture-corpus.py`), and 64-bit sparse-file index tests beyond 16 TB. (#154)
- Added `docs/V1_CLAIM_LEDGER.md` to track v1.0 performance, behavioral, accessibility, and platform targets alongside measurement criteria and evidence status. (#152)
- Added native Linux ARM64 (`ubuntu-24.04-arm`) and macOS Intel (`macos-15-intel`) CI jobs plus strict Clippy checks on Linux CI. (#153)
- Published **[Quarry 0.1.0 beta 3](docs/releases/v0.1.0-beta.3.md)** (build 167, tag `v0.1.0-beta.3`, clean `f5402e4`), Developer ID signed, notarized, and stapled for Apple Silicon macOS, with the build 167 verification report and updated release documentation. (#147)

### Changed

- Hardened `scripts/benchmark-public-datasets.py` to verify input SHA-256 hashes, record output digests and peak RSS/scratch usage, run duplicate-removal benchmarks, and support `--threads`, `--repeats`, and `--expected-outputs`. (#155)
- Simplified **File → Join Files…** into a single-screen dialog with side-by-side file cards, compact join options, quick output-column controls, collapsed **Advanced** overrides, and one-pass join execution. Updated `docs/DESIGN.md` and `docs/USER_GUIDE.md`. (#146)

## [2026-11-20]

### Added

- Added bounded read-only access to gzip-compressed delimited files (`.gz`) across `quarry-core`, the desktop app, and the CLI (`open`, `inspect`, `viewport`, `search`, `filter`, `export`, and `convert-utf8` with `--temp-dir` and `--max-decompressed-bytes`). Opening a `.gz` file shows the first rows immediately from an in-memory bootstrap, then decompresses once into an owner-only temporary copy while building the structural index and verifying every member (header, deflate, CRC-32, ISIZE). Trailing garbage, decompression bombs, and `.gz`/`.tgz` output paths are rejected; **Save As** and **Save As UTF-8** write uncompressed copies; **Inspect File** locates malformed text inside valid gzip streams; and decompression counts against the two-operation limit with cleanup on cancel, failure, tab close, or quit. `Info.plist` registers gzip archives with the Viewer role, third-party notices and review hash were updated, and ADR 0012 plus 18 GiB beyond-RAM acceptance evidence were recorded. (#145)
- Added a **Find all columns** scan in **Columns…** to scan an entire file (including filtered-out rows) for wider rows while preserving column order, hidden columns, and edits, bounding automatically added view columns at 65,536. (#143)
- Added this hand-maintained `CHANGELOG.md`, backfilled from project history. (direct commit)

### Changed

- Replaced the manual **Find all columns** scan with automatic column discovery during background indexing as soon as a file opens. The footer shows the discovered column count as indexing progresses, wider rows append columns to the grid automatically even with filters active, and a completed index confirms the full count (capped at 65,536 columns in the view). (#144)

## [2026-11-16]

### Security

- Capped text-shaping layout sections (`MAX_LAYOUT_SECTIONS = 64`) when rendering cell whitespace indicators in `apps/quarry-egui/src/whitespace.rs`, preventing alternating marked and unmarked characters in a single cell preview from generating thousands of layout sections while keeping textual whitespace marks intact. (#142)

## [2026-11-05]

### Changed

- Locked Quarry to its light appearance even when macOS uses Dark mode, removing the dark palette and setting AppKit's `NSAppearance` to Aqua so file pickers and alerts stay light too. Updated `docs/DESIGN.md`, `docs/USER_GUIDE.md`, and `docs/CONTRIBUTING.md`. (#141)

## [2026-11-04]

### Added

- Redesigned the desktop UI with a native macOS look: unified toolbar and title bar, native macOS menu bar wired to shortcut events, system fonts with equal-width tabular digits for numeric cells, stepped **Join Files** dialog cards, and refreshed tabs, grid, status bar, empty state, notices, popovers, and dialogs. Documented the visual system in `docs/DESIGN.md`. Also fixed grid scrollbar thumb sizing, numeric alignment for exponent and edge-whitespace values, menu enablement for **Reload from Disk** and **Combine Files**, and unsaved-changes prompt focus order. (#139)

### Fixed

- Document tab VoiceOver accessible names now announce "unsaved changes" and "busy" in words instead of the visual `●` and `⋯` markers. (#140)

## [2026-10-19]

### Added

- Added **Join Files** to join two saved or current unsaved delimited files on one to sixteen matching key column pairs, with four join modes, duplicate-key expansion and exact count review, output column projection, bounded disk-spill processing, and guarded UTF-8 publication. (#137)

## [2026-10-18]

### Added

- Added **Inspect File** and validated recovery copies to locate malformed records in delimited files (including aligned UTF-16 records) and write verified recovery copies while preserving standalone inspection state across opens and quits. (#117)
- Added broader character-encoding support for Unicode (including UTF-8 and UTF-16 with native BOM handling) and legacy CSV encodings, preserving Auto detection and source signatures across saves, filters, deletions, and sorts. (#108)
- Added stable multi-column sorting with accessible sort levels and bounded edited-row key generation. (#106)

### Changed

- Filters now evaluate unsaved working cell values without saving first and refresh edited results in place, preserving the active filtered page, cell editor focus, and filtered export safety. (#107)
- Extracted shared private spill storage in `quarry-core` to support bounded file joining. (#129)

### Security

- Hardened CSV exports, staged outputs, and temporary sort and duplicate spill files so data remains owner-private before and after publication, stripping inherited macOS sharing ACLs during staging while preserving destination ACLs when replacing files. (#118)

## [2026-10-17]

### Added

- Added **Clean Up Columns** for bulk cleanup of selected columns (trimming surrounding Unicode whitespace, uppercase and lowercase conversion, and filling empty cells) with sampled previews, exact changed-cell counts, safe working copies, and Undo/Redo. (#101)
- Added subtle whitespace indicators in grid cells with a remembered all-whitespace view toggle. (#103)
- Added **Collapse repeated spaces** to **Clean Up Columns** and clarified whitespace trimming options. (#105)

## [2026-10-10]

### Added

- Added row and column insertion at a chosen position and count (with optional new column names) in the desktop app and bounded core engine, with edits, history, progress, cancellation, and guarded saves. (#95)
- Added explicit **Date / time** sorting with documented parser contracts and cancellation support. (#94)
- Added independent document tabs (up to eight files) with per-tab edits, selection, filters, background jobs, safe per-tab close and quit, native Finder working-file ownership, and APFS-aware pending output path reservations. (#93)
- Added **Combine Files** to combine two or more matching saved files in chosen order into a new file with record validation, progress, and cancellation; also fixed delimiter detection for unterminated records and preserved long-record schemas and trailing carriage returns. (#92)
- **Save As…** now writes reordered display columns in their current grid order. (#91)

## [2026-10-06]

### Added

- Published **[Quarry 0.1.0 beta 2](docs/releases/v0.1.0-beta.2.md)** (build 140, tag `v0.1.0-beta.2`), signed, notarized, and stapled for Apple Silicon macOS, with candidate verification reports and updated release docs. (#87, #89)
- Added complete horizontal and vertical cell grid lines, including headers and row numbers, to the desktop viewer. (#82)

### Fixed

- Fixed long cell values truncating at 120 characters after column expansion or auto-fit; grid previews now extend up to 16,384 characters while copying and editing keep the full value. (#84)
- Stabilized the filtered-export accessibility test against filter-worker timing races and recorded VMPal beta installation evidence. (#85)

## [2026-10-05]

### Added

- Published **[Quarry 0.1.0 beta 1](docs/releases/v0.1.0-beta.1.md)** (build 127, tag `v0.1.0-beta.1`), signed and notarized for Apple Silicon macOS, and fixed Cargo workspace member discovery in third-party notice freshness checks. (#52)
- Added beta identity indicators and **File menu → About & Feedback…** showing version, build, and source revision, copying build details, and linking to the bug form and user guide. (#53)

### Changed

- Made Quarry fully free and open source under MIT OR Apache-2.0, with all application features available in source builds and free official packaged releases. (#51)
- Connected the GitHub Wiki and centralized all future roadmap items and ideas in GitHub Projects. (#59, #80)

## [2026-09-20]

### Changed

- Documented reproducible public CSV benchmarks (`docs/BENCHMARK_DATASETS.md`) and recorded installed desktop validation and keyboard checks for build 118. (#48, #49, #50)

### Fixed

- Guarded native macOS quit (`Cmd+Q` and menu Quit) so Quarry prompts instead of discarding unsaved changes. (#49)

## [2026-09-11]

### Changed

- Improved large-file sort progress feedback and storage review, reused existing row indexes instead of re-indexing before sorting, and rejected stale source indexes before handoff. (#47)
- Ignored local benchmark CSV files and the optional GPUI comparison directory in `.gitignore`. (direct commit)

## [2026-09-10]

### Changed

- Refined toolbar panels with compact, consistent controls and clarified public beta scope and validation documentation. (#46, direct commit)

## [2026-09-08]

### Changed

- Recorded signed beta candidate build 105 validation evidence. (#44)

### Fixed

- Fixed macOS source-change warnings firing after non-content file metadata updates. (#43)

## [2026-09-07]

### Changed

- Polished the **Filters**, **Columns**, **Sort**, and **Duplicates** dialogs. (#40)
- Added beta release checks, third-party dependency license notices, and macOS signing and notice audits; honored the configured Rust toolchain in macOS preflight checks. (#41, #42)

## [2026-09-06]

### Added

- Added temporary storage location controls, disk-capacity preflight checks, and responsive storage review during background jobs. (#38)

### Changed

- Closed out pre-beta validation checklists and reconciled user and packaging guides. (#39)

## [2026-09-05]

### Added

- Added **Find & Remove Duplicates** by selected columns with extra-row count review and explicit keep-first removal. (#37)
- Added exact numeric filters and inclusive **Between** range rules. (#36)
- Added bounded **Undo** and **Redo** for individual cell and header edits. (#35)

## [2026-09-04]

### Added

- Added **Number**, **Character count**, **Word count**, **Shuffle**, and **Reverse** sorting modes. (#34)
- Added **Delete Selected Rows**. (#32)

### Fixed

- Fixed column auto-fit on files with more than 64 columns and kept the start of oversized fitted columns visible. (#33)

## [2026-09-01]

### Changed

- Refreshed roadmap documentation and planned row deletion. (#31)

## [2026-08-27]

### Changed

- Completed the desktop toolbar redesign with a compact file menu, compact format controls, consolidated column and filter actions, contextual Find & Replace controls, footer job progress, a stable toolbar row, a centered file drop zone, and updated user guide documentation. (#29, #30)

## [2026-08-25]

### Added

- Added the Quarry User Guide (`docs/USER_GUIDE.md`). (#27)

### Changed

- Improved filter grouping (same-column alternatives combined with AND across columns) and case-matching controls. (#26)

## [2026-08-23]

### Added

- Added right-click cell context actions to keep or exclude a cell's exact value. (#25)
- Added **Does not equal** filtering. (#24)
- Added 12 GB and 50 GB workflow benchmark documentation. (direct commit)

### Changed

- Accelerated large-file external sorting performance. (#23)

## [2026-08-22]

### Changed

- Refined the compact desktop grid layout, column interactions, and bounded wide-grid rendering. (#22)

### Fixed

- Fixed opening CSV documents directly from macOS Finder. (direct commit)

## [2026-08-21]

### Added

- Added direct **Split** and **Combine** column transformations with editable materialized results, structural undo and redo, improved column selection, and column ruler dividers. (#18)
- Completed direct editing and stable row sorting with hardened validation and edit safety. (#19)
- Hardened macOS desktop packaging, verification, and installation in `scripts/macos-app.sh`. (#20)

### Changed

- Softened desktop accent colors. (#21)
- Added a contributor license agreement (`docs/CLA.md`) and pull-request CLA check, later removed in #51 when Quarry moved to MIT OR Apache-2.0 without a CLA. (direct commits)

## [2026-08-19]

### Added

- Added direct cell editing in the desktop grid. (#17)

## [2026-08-18]

### Added

- Added atomic in-place **Save** (`Cmd+S`) and **Save and Close**, preserving permissions and detecting metadata-visible source changes. (#16)

## [2026-08-17]

### Added

- Added inline header editing and safe no-clobber **Save As**. (#15)
- Added safe cancellable filtered export. (#13)
- Added Quarry branding assets. (#14)

## [2026-08-16]

### Added

- Added bounded streaming filters, multiple filter predicates, and numbered columns. (#11, #12)
- Added the adaptive dense grid and bounded column visibility, width, and reorder controls. (#8, #10)
- Added bounded clipboard copying and bounded streaming search. (#5, #6)

### Changed

- Refreshed roadmap and architecture documentation. (#7)

## [2026-08-15]

### Added

- Added continuous virtual scrolling with adaptive viewport buffering and viewer file controls. (#2, direct commit)

### Changed

- Reduced live-index viewport latency and contention during background indexing. (#3)

### Fixed

- Fixed page navigation controls. (#4)

## [2026-08-14]

### Added

- Established the progressive large-file delimited engine, background indexing with live navigation, viewport benchmarks, the egui desktop viewer selection after UI bake-off, and AI-assisted development disclosure. (direct commits)
