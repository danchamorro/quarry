# Local desktop validation: build 118, 2026-09-20

**Status: completed desktop QA checkpoint; manual release checks remain.**
Both public datasets passed progressive opening/navigation, full numeric-sort
validation, filtered export and large Save As. Undo/Redo restored source/sorted
views, running-sort cancellation recovered cleanly, low-space rejection/recovery
worked, and both original files and disposable copies retained their hashes.
The checks exposed an unsaved Command-Q defect; its fix passed native
save/quit/reopen regression checks in a separate clean build 119. That build
also passed the wide-column and numeric-edge checks below. Drag-and-drop,
VoiceOver and full compact-window acceptance are not marked as passed.

| Environment | Value |
|---|---|
| Application | Quarry 0.1.0, build 118 |
| Installed revision | `38692bb31efdab318dcc66d9556b658d77705c71` |
| Installed source status | `clean` |
| Operating system | macOS 27.0 (26A428), ARM64 |
| Evidence scope | Local installed-app desktop checks |

This report does not replace the frozen signed build 105 candidate in the
[beta release checklist](../BETA_RELEASE_CHECKLIST.md). It does not establish
supported-system scope, fresh-environment acceptance or release authorization.
The [September 11 CLI benchmarks](2026-09-11-public-datasets.md) remain separate
historical measurements. These desktop checks are not controlled performance
benchmarks.

## Fixtures and verification

Synthetic fixtures include a five-row/six-column workflow CSV and a
twelve-row/seventy-column edge CSV with quoted commas, an embedded newline,
Unicode, duplicate values, blank and missing fields, and numeric boundaries.
Current original fixture bytes match the captured baselines. Generated small
outputs were independently compared byte-for-byte with expected LF CSV.

Large inputs are disposable copies of the two
[public benchmark datasets](../BENCHMARK_DATASETS.md). Independent streaming
CSV scans prepared expected filtered records and numeric-sort checks. The
verifier self-check covers numeric interpretation, stable ties, ordering,
truncation, mutation and quoted/newline records. Preparing these expectations
does not establish a successful desktop operation.

| Dataset | Bytes | Data rows | Columns | Desktop status |
|---|---:|---:|---:|---|
| Hacker News | 12,718,039,463 | 28,737,557 | 15 | Opening/navigation, filter/export, sort cancellation, full sort validation and exact-record Save As passed |
| Taiwan weather | 9,770,967,575 | 131,985,329 | 30 | Progressive open/navigation, filter/export, low-space recovery, full sort validation and exact Save As passed |

Local evidence is retained under the ignored repository-relative directory
`target/validation/2026-09-20-desktop118/`. It includes
`desktop-evidence.md`, fixture baselines, output verification JSON,
`resources.jsonl` and independent oracle files. These local files are not
published by this report.

## Completed small-file checks

| Workflow | Observed result |
|---|---|
| Open and edit | Native picker and Finder opening worked. Cell and header edits each passed Undo/Redo. Exact Save As retained Unicode, quoted delimiters and multiline data; source bytes stayed unchanged. |
| Filter and export | Amount Between 100 and 1000 inclusive AND Status Equals Active returned 3 rows. `NaN` and reversed bounds disabled Apply without replacing the active filter. Clear restored all 5 rows. Export matched exact expected bytes. |
| Sort | Numeric Amount ascending, Details, Undo/Redo and exact Save As passed with all 5 rows and the header preserved. |
| Duplicates | Name-key review reported 1 extra and 4 to keep. Cancel preserved 5 rows. Repeating and explicitly removing extras passed Undo/Redo and exact Save As. Output retained the first Name occurrence from the current sorted order. |
| Columns | Six-column hide, reset, auto-fit, view persistence and Escape passed. On the edge fixture, searching for and hiding original column 70, Amount, persisted after reopening Columns with 69 shown. Initial reorder attempts had no verified result; reorder subsequently passed on build 119, recorded below. |
| Header setting | Edge-fixture auto-detection selected no header and showed 13 records. Explicit header override correctly showed 12 data rows. This records the heuristic choice and working override for this synthetic fixture; it does not diagnose a detection regression. |
| Window and lifecycle | Native resize kept controls visible in a smaller window, without completing the full compact-window workflow. The later unsaved large-document quit exposed the native termination defect described below; corrected save/quit/reopen passed on build 119. |
| Working storage | Selecting the dedicated monitored working folder succeeded. Weather sort later passed low-space rejection and recovery through the storage dialog. |

Exact small-output identities:

| Output | Data rows | Bytes | SHA-256 |
|---|---:|---:|---|
| Cell/header Save As | 5 | 252 | `be7631323425a934f33b3d07902454678ba8aeb67867a0b31961c3280746bfb3` |
| Compound-filter export | 3 | 157 | `2efc7c8e8d7d296a9b73efdd1d66b848bbef6f1a431aeac9f1ddef830145f318` |
| Numeric sort Save As | 5 | 244 | `33f4f6b1bd5566520a53bb4d69deb437fb385a4fd0a654dfd20f21321e5bf679` |
| Keep-first duplicate removal | 4 | 198 | `9974d13dc81f5c4375fbbe039422762d9460e0186f30d0d14cec88d47f5a622c` |

## Hacker News desktop checks

Finder-open displayed rows 1-54 while indexing was at 56.4 percent with
16,271,418 rows discovered. Indexing completed at 28,737,557 data rows. Format
reported Comma and Header row. Keyboard row jumps reached middle row
14,368,778, ID `20119562`, and last row 28,737,557, ID `7536640`, matching the
independent source scan. Native window zoom displayed all 15 source columns.

The desktop filter `type = story AND score >= 100` exported **128,222 data
rows**. `hacker-news-filtered-verification.json` records equality of every
decoded CSV record in order, including the header, against the independent
expected export. This is exact record comparison, not a raw-byte identity claim.
At that preservation checkpoint, the disposable source still had
12,718,039,463 bytes and SHA-256
`165d9336b6266519da4e99d0348576537f80337e40d1794aefb00258a49a0373`,
matching the original.

A numeric sort was cancelled after the reading phase reached 13.7 percent.
The UI reported that the sort was cancelled and the document was not changed.
Page Down then displayed rows 55-108 of 28,737,557. The selected scratch folder
contained no files after cancellation. These observations are recorded in
`hacker-news-cancel-cleanup.json`; they establish a running-operation
cancellation and usable navigation, not just dismissal of the Sort dialog.
The full numeric-sort rerun subsequently completed, approximately two minutes
after it began by desktop observation. This is an approximate observation, not
a controlled worker-time measurement. Undo visually restored the source view,
and Redo restored the sorted view. The initial sorted version was lost during
the subsequent unsaved quit described below. A rerun completed and was saved as
`hacker-news-sorted.csv`. `hacker-news-sorted-verification.json` validates all
**28,737,557 data rows**, the header, ascending numeric keys, and all **1,761
numeric groups'** counts and decoded-row sequence hashes. Retained records and
stable ties matched the independent source scan under the SHA-256 collision
assumption.

The saved file has **12,718,039,433 bytes**, SHA-256
`fbf4d9e4e1b3befbc4cdecd5067b5642de18bd108940f92bbb062daa17222adc`.
It is 30 bytes shorter than the copied completed sort because its 15 header
fields no longer have unnecessary quotes. The raw-byte identity check failed,
and that evidence is retained. The full decoded-record comparison subsequently
passed for all 28,737,557 rows in order, including the unchanged decoded header,
as recorded in `hacker-news-save-as-record-verification.json`. Save As passed
record preservation, while original header quoting was normalized.

During the first completed sort, before the successful rerun, Command-Q
terminated the process while the sorted document was unsaved, with no
confirmation observed. The selected scratch folder was empty afterward.
This exposed the native termination defect described below, rather than passing
unsaved-work handling or lifecycle acceptance.

## Weather desktop checks and storage recovery

Native opening displayed the first 72 rows while indexing was at 13.2 percent,
with 16,897,203 rows discovered. An early jump to the midpoint safely reported
that the target was not indexed yet. After indexing completed at 131,985,329
rows, navigation reached row 65,992,664 with StationId `C1V290` and blank RH,
and final row 131,985,329 with StationId `C1D420` and RH `-99.8`. Those visible
fields matched the independent oracle. Date fields were truncated in the grid;
their full displayed values were not verified.

The desktop filter `StationId = C0X100 AND RH Between 70 and 80` inclusive
exported **26,505 data rows**. `weather-filtered-verification.json` confirms
every decoded CSV record in order, including the header, matched the independent
expected export.

A weather numeric RH sort reviewed an 83.12 GiB conservative allowance against
a selected volume with only 62.46 MiB available. The dialog reported more disk
space was needed and disabled Continue. Selecting the monitored working folder,
with 770.55 GiB available at that check, enabled Continue. This establishes
preflight insufficient-space rejection and recovery, not exhaustion during an
active write. `low-space-ui-result.json` records the check. The complete sort
then completed. `weather-sorted-verification.json` verifies all **131,985,329
data rows**: the header, ascending numeric keys, and all 135 numeric groups'
counts and decoded-row sequence hashes match the independent source scan. This
proves retained records and stable ties under the SHA-256 collision assumption.
Undo visually restored the source view with initial RH `72`; Redo restored
the sorted view with initial blank RH. These observations do not separately
hash the complete Undo and Redo states.

Desktop Save As produced `weather-sorted.csv`, **9,770,967,575 bytes**, SHA-256
`4be7c66945d56ececb1564c08ab94f470b992db8cbdedd49f4e18ceeb216d58b`.
Its whole-file hash matched the copied completed desktop sort. The UI switched
to the saved path as a clean document and reindexed it; the selected scratch
folder was empty. `weather-save-as-verification.json` and
`weather-save-cleanup.json` record these checks.

## Source preservation and observed resources

`final-source-preservation.json` records full SHA-256 reads of both originals
and both disposable GUI copies, completed at 20:25:22 UTC after both complete
sorts, both filtered exports and both Save As validations. All four matched the
original baselines, with size and mtime unchanged during each read.

| Dataset, original and GUI copy | SHA-256 |
|---|---|
| Hacker News | `165d9336b6266519da4e99d0348576537f80337e40d1794aefb00258a49a0373` |
| Weather | `b775c8b7e6d39643bc1e10874deecc9b067e2866c79a8e647a6c93550412d47f` |

The following maxima were observed in the named sort phases of
`resources.jsonl`; `finalresources-summary.json` records the final summary and
`hn-fullsort-resources.json` provides detail for the first Hacker News pass.

| Sort phase | Samples | Process RSS, MiB | Scratch files | Logical scratch, GiB | Allocated scratch, GiB |
|---|---:|---:|---:|---:|---:|
| Hacker News first full pass | 189 | 208.23 | 803 | 23.86 | 23.86 |
| Weather | 199 | 299.59 | 828 | 21.71 | 21.88 |
| Hacker News validation rerun | 145 | 304.97 | 806 | 24.17 | 24.18 |

A single 12,718,039,463-byte sorted backing file remained after the first
Hacker News scratch consolidation while the document needed that version.

The phase label covered 188.79 seconds including setup and idle time. Scratch
creation through consolidation occupied approximately 75.3-77.3 seconds by
one-second samples; this is not worker time or a strict completion-time bound.
Sampling is not atomic, can miss higher peaks, and process RSS excludes the
system file cache. These observations do not establish cold-cache performance
or a controlled comparison with the CLI benchmarks.

Monitoring covered the installed build 118 executable only, excluding the
build 119 test bundle. Scratch measurements cover the chosen working folder,
excluding datasets and output destinations. At completion, the retained scratch
files from both Save As operations were gone, the folder was recursively empty,
and the monitor had stopped. The temporary low-space image was verified and
detached, with its image file preserved; `low-space-cleanup.json` records that
bounded cleanup.

## Native quit defect and follow-up build

Build 118's native application termination path bypassed the guarded viewport
close path, allowing Command-Q to terminate an unsaved document without the
confirmation observed for guarded closes. Source commit `f65ebb5` routes native
quit through the unsaved-work guard. Source validation passed 315 workspace
tests, strict Clippy and formatting; review found no remaining actionable issue.
A temporary packaged **clean build 119**, revision
`f65ebb5a7d2cd8fcd5f913bf9db740245fb257a7`, then passed native Command-Q and
application-menu Quit confirmation. Keep Editing retained the unsaved
`CaseyQuit` value. Save and Close exited and wrote the exact expected bytes,
SHA-256 `3d9ff670d859601372671844db803d34bedf6366443f8849acda73ab8d8fb51e`.
The process exited, and reopening the saved file read back `CaseyQuit`.
`native-quit-regression.json` records the complete bounded regression. The fix
is in [PR #49](https://github.com/danchamorro/quarry/pull/49), with merge and
installation still separate at this evidence checkpoint. This test bundle has not replaced
the installed main build. Earlier build 118 findings retain their original
revision and are not reassigned to build 119.

## Build 119 wide-column and edge checks

On the six-column fixture, original Status column 3 was moved to the first
displayed position. The change persisted after closing and reopening Columns;
Reset restored original order 1, 2, 3 and onward.

On the seventy-column fixture, Amount remained hidden as original column 70.
Its searchable filter picker still selected that column correctly. Amount
Between 100 and 1000 inclusive AND Status Equals Active exported exactly IDs
**1, 2, 11, 12**, in order and with all 70 columns. This exercised endpoints,
an equivalent scientific number, blank/missing fields and invalid numeric data.
Bounds `9007199254740993` through `9007199254740993` matched only ID 10,
excluding ID 9's adjacent integer `9007199254740992`.

Clear restored 12 rows, Reset restored 70 shown columns, Auto-fit expanded Text,
and horizontal navigation reached column 70 and its numeric values. Attempting
Number sort on Amount reported invalid `NaN` at data row 8, column 70, leaving
the document clean and unchanged. Dismissing the error restored usable controls.
`edge-wide-results.json` records the exact export and original fixture hash.

## Remaining manual release checks and observation limits

| Area | Remaining evidence |
|---|---|
| Drag-and-drop | Native drag-and-drop opening was not run; picker and Finder opening do not establish it. |
| Accessibility and keyboard | VoiceOver and a complete keyboard-only workflow remain untested. Observed row-jump, Page Down, Undo/Redo and Escape checks are bounded coverage. Intermittent wide-grid accessibility loss remains an open observation. |
| Compact window | Earlier resize rendered visible controls, but a later resize attempt did not complete. The full compact-window workflow remains untested. |
| Broader release acceptance | Other supported-system versions, fresh-environment distribution acceptance and owner approval remain governed by the beta checklist. The run exercised Number sorting; it does not revalidate every other Sort mode. |

Large-file Undo/Redo was verified through observed source/sorted views; separate
whole-file hashes of every intermediate history state were not captured. The
full sorted outputs and final saves were independently checked as described
above. This evidence checkpoint covers the completed automated/native checks,
without assigning passes to the manual checks that were not run.

The edge-wide fixture did not consistently expose egui accessibility content
after native or Finder opening, although coordinate interaction worked.
Intermittent ScreenCaptureKit `-3811` / `noWindowsAvailable` errors also affected
desktop observation and coordinate input while Quarry remained running.
Keyboard input, native window actions and some screenshots remained available.
Gracefully quitting and reopening the narrow fixture through Finder recovered
full accessibility and coordinate control, allowing subsequent checks to
continue. On build 119, accessibility content could disappear with all 12 wide
records visible and return after filtering to four. The cause is unverified;
this is not proof of an application crash. Accessibility acceptance remains open.
