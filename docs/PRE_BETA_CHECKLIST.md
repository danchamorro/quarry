# Priority Checklist Before Beta

This is the focused product checklist to complete before Quarry's first public
beta. Work in priority order: individual edit Undo, numeric filters, duplicate
cleanup, then temporary-disk handling. All four feature priorities are complete,
merged, and locally validated. The final connected workflow passed in the clean
merged app; owner review remains pending below.

Mark an item complete only after its behavior is implemented and validated.
Record the PR and validation evidence under each priority, and update the
[user guide](USER_GUIDE.md) and [roadmap](ROADMAP.md) as work lands.

## 1. Undo individual cell and header edits

**Goal:** change several values, then undo or redo those changes one at a time.
Individual committed cell/header history now works alongside the existing
adjacent whole-file working version.

- [x] Add Undo and Redo for committed cell values and header names, including
  repeated edits to the same cell.
- [x] Make the commands available through visible controls and standard
  keyboard shortcuts, with correct focus behavior while typing in a cell.
- [x] Clear obsolete Redo entries after a new edit. Define and document how
  edit history interacts with whole-file operations, Save, Save As, Discard,
  and opening another file.
- [x] Keep history within a documented resource limit without copying the
  entire CSV for each cell edit.
- [x] Validate mixed cell/header edits, multiline values, Undo/Redo after
  structural operations, and source preservation. Verify the workflow in the
  installed app and update the user guide.

**Evidence:** [2026-09-04 validation](benchmarks/2026-09-04-individual-edit-undo.md)
on `codex/individual-edit-undo`: eight focused regressions, all 256 workspace
tests, strict Clippy, formatting, release build, and installed-app validation
passed. Mixed/repeated edits, typing focus, structural Undo/Redo, history limits,
lifecycle resets, and exact source/output bytes were checked. CodeRabbit CLI
0.7.5 completed its review with zero findings on 2026-09-05. Implementation
commit `55ad8df` merged in
[PR #35](https://github.com/danchamorro/quarry/pull/35) as `cd05013`.
The installed app was updated and verified from that clean merged commit;
cell and header Undo/Redo passed again in the reopened app. Controls
and limits are documented in the [user guide](USER_GUIDE.md#undo-and-redo-changes)
and [architecture](ARCHITECTURE.md#document-editing-and-persistence).

## 2. Numeric filters

**Goal:** answer questions such as "Balance greater than 500" and "Amount
between 100 and 1000." Extend Contains, Equals, and Does not equal with exact
numeric comparisons and inclusive Between.

- [x] Add numeric greater-than, greater-than-or-equal, less-than,
  less-than-or-equal, and inclusive Between filters.
- [x] Reuse the exact number interpretation used by Number sorting, including
  decimals and scientific notation, without floating-point rounding.
- [x] Define blank, missing, and invalid-value handling and reject invalid
  filter bounds clearly. Between must require both bounds to match.
- [x] Integrate numeric rules with existing grouped filters and filtered
  export while preserving current text-filter behavior.
- [x] Validate numeric boundaries, precision, combined rules, export results,
  cancellation, and bounded memory on a large-file workload. Verify the
  installed-app workflow and update the user guide.

**Evidence:** [2026-09-05 validation](benchmarks/2026-09-05-numeric-filters.md)
on `codex/numeric-filters`: all 263 workspace tests, strict Clippy, formatting,
release build, and 1 GB engine validation passed. All five numeric match counts
and every raw exported record matched an independent exact-decimal oracle;
source hashes, cancellation, and bounded index memory passed. The
[user guide](USER_GUIDE.md#numeric-values-and-bounds) documents number syntax,
invalid-value handling, and combined rules. The installed feature build passed
Between with a text rule, invalid-bound rejection while preserving the active
filter, exact exported bytes, and source preservation. It records dirty source;
CodeRabbit CLI 0.7.5 completed review with zero findings across all 14 changed
files on 2026-09-05. Implementation commit `d9f47dd` and review follow-ups merged
in [PR #36](https://github.com/danchamorro/quarry/pull/36) as `8ab5185`. All checks
passed. The installed app was updated from the clean merge commit and inclusive
Between returned the expected rows again in the reopened app.

## 3. Find and remove duplicates

**Goal:** identify repeated records using selected columns, review the count,
and explicitly remove extra occurrences while keeping the first row.

- [x] Let users choose the columns that determine whether records match.
  Define case sensitivity and how blank or missing fields compare.
- [x] Show the duplicate count before removal and explain that the first
  occurrence in the current row order will be kept.
- [x] Keep every retained row intact and preserve its relative order. Keep
  the header fixed and account for current unsaved values.
- [x] Reuse the working-copy, Undo/Redo, Save, Save As, and Discard workflow.
  Cancellation or failure must preserve the current document and source.
- [x] Validate selected-column matching, repeated identical rows, quoted and
  multiline values, exact retained rows, cancellation, temporary-file cleanup,
  and bounded memory on a large-file workload. Verify the installed-app
  workflow and update the user guide.

**Evidence:** [2026-09-05 validation](benchmarks/2026-09-05-find-remove-duplicates.md)
on `codex/find-remove-duplicates`: all 276 workspace tests, strict Clippy,
formatting, release build, and 1 GB exact-output validation passed. The installed
feature build passed selected-column matching with an unsaved edit, reviewed
counts, Cancel, explicit removal, Undo/Redo, and exact Save As output with an
unchanged source. Implementation commit `6f0eeb4` and documentation follow-up merged in
[PR #37](https://github.com/danchamorro/quarry/pull/37) as `9210369`. All checks
passed. The clean merged app was installed and verified; duplicate removal and
Undo passed again, with the source unchanged.
Matching and workflow details are in the
[user guide](USER_GUIDE.md#find-and-remove-duplicates) and
[architecture decision](adr/0004-bounded-duplicate-removal.md).

## 4. Temporary-disk handling

**Goal:** explain storage requirements before a large operation and let users
use a drive with enough space. Apply shared capacity checks to working copies,
sort/duplicate runs, Save, and export while retaining atomic publication.

- [x] Allow users to choose a temporary working location for large operations.
- [x] Check available space on the relevant volume before starting, accounting
  for temporary output and retained working versions. Keep atomic Save staging
  on the destination volume.
- [x] Show required and available space clearly. Handle an unavailable,
  unwritable, or insufficient-space location with an actionable message.
- [x] Handle space running out after the check without publishing partial
  output or losing the current document. Preserve required Undo files and
  remove unpublished temporary output on cancellation or failure.
- [x] Validate insufficient space, write failure, cancellation, and a selected
  alternate working location. Verify the installed-app workflow and document
  storage requirements and cleanup behavior.

**Evidence:** [2026-09-05 validation](benchmarks/2026-09-05-temporary-disk-handling.md)
for implementation commit `6e91c39` in
[PR #38](https://github.com/danchamorro/quarry/pull/38): all 290 workspace tests, strict Clippy,
formatting, locked release build, and deterministic 1 GB before/after checks
passed. Sort and duplicate outputs were byte-identical; cancellation removed
unpublished output. A separate 64 MiB HFS+ test volume verified actual
insufficient capacity independently on scratch and output volumes. Automated
late-write failures preserved the source and required Undo files.

The installed feature build passed folder selection, actionable failure,
required/available space, low-space blocking, Cancel/Continue, exact 1 GB sort,
working versions and Undo/Redo across volumes, atomic Save, and cleanup. Its
metadata records dirty source from validation before commit. The recorded
1 GB, cross-volume, and installed-app checks precede the final private-output
handoff fix. The handoff fix and storage-review polling fix (`01c1503`) passed
all 291 workspace tests and merged in PR #38 as `f5efee2`. The clean merged app
was installed and verified; a native smoke check passed editing, sort,
Undo/Redo, and exact Save As output with the original source unchanged. This
smoke check does not repeat the earlier 1 GB or cross-volume measurements.
Controls and limits are documented in the
[user guide](USER_GUIDE.md#temporary-storage-and-free-space) and
[ADR 0005](adr/0005-temporary-storage.md).

## Completion and release handoff

- [x] All four priorities have linked implementation and validation evidence.
- [x] Review a connected workflow in the installed app: edit, Undo/Redo,
  filter, export, remove duplicates, and save. Confirm exact output and source
  preservation before Save.
- [x] Reconcile the user guide, roadmap, and this checklist with the merged
  behavior.
- [ ] Complete owner review of the final pre-beta results.

**Connected-workflow evidence:** the
[2026-09-06 closeout validation](benchmarks/2026-09-06-pre-beta-closeout.md)
passed in the clean installed app at `f5efee2`: cell/header editing and
Undo/Redo, Save As, numeric Between filtering, exact filtered export, reviewed
duplicate removal and Undo/Redo, and final Save. Independent byte comparisons
confirmed every output, the unchanged original source, and preservation of the
Save As checkpoint until explicit Save. Private working files were cleaned up.

Finishing this checklist means the priority product work is complete. A public
download still needs release preparation, including supported-system testing,
Developer ID signing, notarization, and license notices. Track the remaining
gates and final owner acceptance in the [beta release checklist](BETA_RELEASE_CHECKLIST.md),
with packaging procedures in the [macOS packaging guide](MACOS_PACKAGING.md).

## Interface polish follow-up

The four functional priorities above remain complete. Interface polish merged
in [PR #40](https://github.com/danchamorro/quarry/pull/40) as `69d5f15` using the
existing egui desktop. The clean merged build was installed and verified with
native checks. Columns and Filters remain movable tool windows; Sort and Find
Duplicates remain modal dialogs. The shared
visual treatment uses consistent spacing, neutral surfaces, clear headers and
footer actions, and the existing accent color for primary actions.

Filters adds a searchable source-column picker, adjacent Between bounds, inline
validation, scrollable rules with visible footer actions, and expandable matching
details. Closing the tool window preserves draft rules and the active filter.

- [x] Validate the redesigned Filters window in the native app. Checked name
  and source-number search, adjacent range bounds, Tab navigation, numeric
  filtering and Clear, menu dismissal, contrast, and moving the window.
- [x] Pass the initial Filters checks: formatting, workspace Clippy, all 294
  workspace tests, the release build, and local bundle validation. Interaction
  regressions cover multiline
  values, hidden/reordered source columns beyond 64, Escape and draft
  preservation, picker height after searching and reopening, and footer bounds
  at 860 by 540 with active and blocked states.
- [x] Complete owner review of Filters before extending the design. The owner
  approved the update and requested the remaining dialog improvements.
- [x] Validate Columns with aligned full-width rows, drag handles, original
  source-column numbers, search, visibility, reorder, Reset, and Auto-fit.
- [x] Validate Sort with a compact mode picker, direction buttons, Text-only
  Match case, visible selected-column and storage information, and expandable
  Details. Preserve all six sorting modes and cancellation.
- [x] Validate Find Duplicates and its review with selected-column and
  keep-first summaries, matching Details, reviewed counts, explicit removal,
  cancellation, and Undo/Redo.
- [x] Pass formatting, strict workspace Clippy, all 296 workspace tests
  (including 115 GUI tests), the release build, and local bundle verification.

**Remaining-dialog evidence:** automated interaction checks cover all six Sort
modes, accessible controls, popup-first Escape, expanded Details and footer
bounds at 860 by 540, and source preservation. Columns checks cover equal row
widths, left-aligned short and truncated names, search, visibility, Reset,
Auto-fit, source-column reorder commands, and Escape. Duplicate review checks
cover visible counts and actions at the same minimum window size, explicit
removal, cancellation, and retained history.

Native checks passed Columns search, hide, Reset, Auto-fit, Escape, and visual
name alignment. Dragging the Note column to the top updated both the list and
grid while retaining source-column number 6; Reset restored source order 1
through 6. Number sorting produced `100, 500, 500, 1000, 1200`; Undo restored
the original order.
Finding duplicates by first name reported one extra row and four rows to keep.
Expanded Details and Cancel left all five rows intact; explicit removal left
four rows, Undo restored five, Redo returned to four, and a final Undo restored
the clean document. The original 274-byte source remained byte-identical.
This native check exercised Number sorting; the other modes have automated
coverage.

A terminal UI remains a future, separate frontend. This work does not implement
it.

## Follow-ups that do not block this checklist

- Date/time sorting remains planned in [Phase 6D](ROADMAP.md#phase-6d-date-and-time-sorting-planned).
- Multi-column sorting.
- Inserting rows and columns.
- Editing and deleting rows while a filter is active.

Keep these separate so the first beta has a clear finish line.
