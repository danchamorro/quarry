# Priority Checklist Before Beta

This is the focused product checklist to complete before Quarry's first public
beta. Work in priority order: individual edit Undo, numeric filters, duplicate
cleanup, then temporary-disk handling. Priority 1 is implemented and locally
validated; priorities 2 through 4 remain planned.

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
lifecycle resets, and exact source/output bytes were checked. The local installed
feature build records dirty source. CodeRabbit CLI 0.7.5 completed its review
with zero findings on 2026-09-05. The feature branch is awaiting PR submission
and has not been merged. Controls
and limits are documented in the [user guide](USER_GUIDE.md#undo-and-redo-changes)
and [architecture](ARCHITECTURE.md#document-editing-and-persistence).

## 2. Numeric filters

**Goal:** answer questions such as "Balance greater than 500" and "Amount
between 100 and 1,000." Current filters support Contains, Equals, and Does not
equal.

- [ ] Add numeric greater-than, greater-than-or-equal, less-than,
  less-than-or-equal, and inclusive Between filters.
- [ ] Reuse the exact number interpretation used by Number sorting, including
  decimals and scientific notation, without floating-point rounding.
- [ ] Define blank, missing, and invalid-value handling and reject invalid
  filter bounds clearly. Between must require both bounds to match.
- [ ] Integrate numeric rules with existing grouped filters and filtered
  export while preserving current text-filter behavior.
- [ ] Validate numeric boundaries, precision, combined rules, export results,
  cancellation, and bounded memory on a large-file workload. Verify the
  installed-app workflow and update the user guide.

**Evidence:** pending.

## 3. Find and remove duplicates

**Goal:** identify repeated records using selected columns, review the count,
and explicitly remove extra occurrences while keeping the first row.

- [ ] Let users choose the columns that determine whether records match.
  Define case sensitivity and how blank or missing fields compare.
- [ ] Show the duplicate count before removal and explain that the first
  occurrence in the current row order will be kept.
- [ ] Keep every retained row intact and preserve its relative order. Keep
  the header fixed and account for current unsaved values.
- [ ] Reuse the working-copy, Undo/Redo, Save, Save As, and Discard workflow.
  Cancellation or failure must preserve the current document and source.
- [ ] Validate selected-column matching, repeated identical rows, quoted and
  multiline values, exact retained rows, cancellation, temporary-file cleanup,
  and bounded memory on a large-file workload. Verify the installed-app
  workflow and update the user guide.

**Evidence:** pending.

## 4. Temporary-disk handling

**Goal:** explain storage requirements before a large operation and let users
use a drive with enough space. Sorting currently estimates required space;
working copies use the system temporary directory without an available-space
check.

- [ ] Allow users to choose a temporary working location for large operations.
- [ ] Check available space on the relevant volume before starting, accounting
  for temporary output and retained working versions. Keep atomic Save staging
  on the destination volume.
- [ ] Show required and available space clearly. Handle an unavailable,
  unwritable, or insufficient-space location with an actionable message.
- [ ] Handle space running out after the check without publishing partial
  output or losing the current document. Preserve required Undo files and
  remove unpublished temporary output on cancellation or failure.
- [ ] Validate insufficient space, write failure, cancellation, and a selected
  alternate working location. Verify the installed-app workflow and document
  storage requirements and cleanup behavior.

**Evidence:** pending.

## Completion and release handoff

- [ ] All four priorities have linked implementation and validation evidence.
- [ ] Review a connected workflow in the installed app: edit, Undo/Redo,
  filter, export, remove duplicates, and save. Confirm exact output and source
  preservation before Save.
- [ ] Complete owner review and reconcile the user guide, roadmap, and this
  checklist with the shipped behavior.

Finishing this checklist means the priority product work is complete. A public
download still needs release preparation, including supported-system testing,
Developer ID signing, notarization, and license notices. Track packaging work
through the [macOS packaging guide](MACOS_PACKAGING.md).

## Follow-ups that do not block this checklist

- Date/time sorting remains planned in [Phase 6D](ROADMAP.md#phase-6d-date-and-time-sorting-planned).
- Multi-column sorting.
- Inserting rows and columns.
- Editing and deleting rows while a filter is active.

Keep these separate so the first beta has a clear finish line.
