# ADR 0007: Independent document tabs

## Context

Users need to switch between large delimited files while retaining unsaved edits,
selection, navigation, filters, and background progress. The production egui app
previously held one document. Independent tabs are the scope of issue #55;
comparison panes and synchronized scrolling remain separate work.

## Decision

Each `DocumentTab` owns its `Document` and presentation state: format overrides,
search and replacement drafts, filter rules, column-manager state, notices,
storage review, and close state. `Document` continues to own the session, bounded
viewport and indexes, selection, edit history, temporary working copies, and
worker jobs. No core engine dependency on tabs is introduced.

The application retains one active tab and a bounded collection of inactive tabs,
with stable IDs. Each frame polls every tab's workers against their owning state.
Only the active tab receives grid input, clipboard actions, shortcuts, and menus.
Switching commits any cell/header draft, surrenders text focus, restores manager
selection and each text field's cursor/Undo state, and namespaces the table's
cached widths and scroll position by tab ID.

Open, Finder requests, command-line paths, and multi-file drops create tabs. A
canonical-path or hard-link duplicate selects the existing tab without resetting
its edits or format. Reload and format reopening remain explicit replacement
operations with unsaved-change and active-operation guards. Save As moves only its
own tab to the saved copy. Destinations belonging to another open tab or an active
writer are rejected, including hard links and normalized new destinations.
On macOS, pending output names also use the target volume's case-sensitivity
capability and canonical Unicode equivalence. Existing file identities remain
authoritative; an unavailable volume capability conservatively rejects names
that differ only by case.

Closing a tab prompts for its unsaved edits and waits visibly for active file
operations, with Keep Editing and cancellation controls. Quit reviews every tab.
Discard approvals during quit do not release documents until the whole quit is
approved, so cancelling a later prompt retains every tab and its Undo history.
Successfully saved files remain saved if quit is subsequently cancelled.
An open Combine Files dialog blocks quit with a visible explanation, retaining
its selections, checked plan and running operation. Closing that dialog or
cancelling its job requires an explicit dialog action before document quit review.

If a completed in-place save cannot be reopened, its stale document is released.
An inactive empty tab is removed and its saved-path/reopen error is shown on the
visible tab, preserving that other document and its history. An active failure
selects a remaining document and carries the error to it. External file moves
after publication can trigger this recovery; the saved bytes remain on disk.

## Resource policy

- At most eight open documents; opening beyond that limit fails without eviction.
- At most two save/export/structural-operation workers across tabs and Combine
  Files. A third operation is rejected with an actionable message. Structural
  analysis also counts; a completed duplicate preview does not occupy a slot.
  Deferred storage reviews recheck this budget before consuming their pending
  operation, retaining the review until a slot is free.
- Indexing, search, filtering, and filtered reads retain their existing per-document
  worker limits. With eight tabs, there are at most eight workers of each kind.
  Each structural/filter checkpoint index retains its existing 16 MiB budget;
  viewport and record-size bounds continue to apply. Sparse unsaved edits and
  selected ranges retain their existing data-dependent memory costs.
- Temporary working files and Undo/Redo files belong to their document. Switching
  tabs retains them; closing that document joins/cancels its workers and releases
  its files. A cancelled quit releases none. Disk checks still run before writes.
- These are bounded counts and per-worker budgets, not a single-file RSS promise
  multiplied across arbitrary simultaneous operations. Release VM measurements
  are recorded separately from deterministic correctness tests.

## Alternatives and consequences

Keeping shared edit/filter state would risk cross-document actions and completion
events. Reopening on every switch would lose history and repeat expensive scans.
An unlimited tab collection would multiply worker and checkpoint costs without a
bound. A global worker scheduler is deferred: two concurrent file operations plus
the document cap provide a small, explicit first resource policy without adding
queued-operation semantics.

The horizontal tab strip provides full-path tooltips, accessible tab and close
names, dirty/activity markers, and Ctrl-Tab/Ctrl-Shift-Tab navigation. Cmd-O opens
files and Cmd-W closes the active tab. The toolbar remains a separate bounded row.
The selected tab is revealed immediately on navigation and window resize.

[Release VM evidence](../benchmarks/2026-10-10-document-tabs.md) records simultaneous
large documents, native cancellation/recovery, source and output hashes, and RSS
observations separately from deterministic tests.
