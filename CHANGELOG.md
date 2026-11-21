# Changelog

All notable changes to Quarry are documented here. This file is maintained by
hand. The format is based on [Keep a Changelog](https://keepachangelog.com/).

Changes are grouped by the day they merged into `main` (US Eastern time).
Tagged beta releases are noted on the day they were published, with exact
checksums and verification details under `docs/releases/`. Numbers in
parentheses are GitHub pull requests.

## [2026-11-21]

### Added

- Added native `ubuntu-24.04-arm` (`linux-arm64-core-cli`) and `macos-15-intel` (`macos-intel-core-cli`) CI jobs plus package-scoped Clippy (`-D warnings`) on `linux-core-cli` in `.github/workflows/ci.yml`, logging the executed host architecture in every CI job and boxing `PredicateMatcher::Text` in `quarry-core` to resolve an `x86_64` `clippy::large_enum_variant` warning. (#153)
- Published **[Quarry 0.1.0 beta 3](docs/releases/v0.1.0-beta.3.md)** (build 167, tag `v0.1.0-beta.3`, clean `f5402e4`), Developer ID signed, notarized, and stapled for Apple Silicon macOS, with the build 167 verification report and updated release documentation. (#147)

### Changed

- Simplified **File → Join Files…** into a single-screen layout: side-by-side **Main file** and **Additional file** cards combine open-tab selection (auto-preselecting up to two idle open tabs) with **Browse file…**, **Key column**, and **Include unmatched Main/Additional rows** checkboxes; a compact options bar provides **Ignore case (A–Z)**, **Include header row**, **+ Add key**, **Swap**, and a live join-type summary; **Output columns** offers **All** / **Main only** / **None** and a vertical **Top** / **Up** / **Down** / **Bottom** rail; saved-file format overrides, unique-key constraints, and column heading renaming move into a collapsed **Advanced** section; and **Join…** scans, writes, and opens the result in a new tab in one pass (pausing only when many-to-many expansion or newly discovered columns require confirmation, with an optional **Preview** button). Updated `docs/DESIGN.md` and `docs/USER_GUIDE.md`. (#146)

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
