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
canonical-path or hard-link duplicate of a document's logical source or current
working file selects its owner without resetting its edits or format. The current
temporary working file never becomes another tab's independently owned source. Reload and
format reopening remain explicit replacement
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

---

## Amendment (2026-11-22, Owner Decision T9): Dormant-tab scheduler, 200-file open, and internal FD budget

### Context

Opening large workspaces (up to 200 files across multiple windows in v1.0) cannot keep every
document's 16 MiB checkpoint index, row buffers, and active readers resident simultaneously,
and macOS defaults to a low soft `RLIMIT_NOFILE` of 256. Owner decision **T9** (`2026-11-22`)
approved lifting the hard 8-tab open limit by introducing a frontend-neutral dormant-tab
scheduler in `crates/quarry-workbench` (`dormancy.rs`) while keeping the global write cap
(`MAX_WRITE_JOBS = 2`) intact.

### Amended decision

1. **Hydration ceiling with multi-window guarantee:** `WorkbenchScheduler` replaces the fixed
   8-open-tab rejection cap at the frontend-neutral workbench layer with a bounded hydrated-tab
   ceiling (`DEFAULT_MAX_HYDRATED_TABS = 8`, automatically raised to at least the number of open
   windows so every window retains `>= 1` hydrated active tab). Unpinned idle hydrated tabs are
   evicted LRU (preferring clean tabs before dirty tabs) into dormant state rather than rejecting
   new opens. In this initial Phase 4A-2 delivery, `apps/quarry-egui` raises `RLIMIT_NOFILE` at
   startup while retaining its legacy 8-document open cap until frontend tab-manager integration
   adopts `WorkbenchScheduler`.
2. **Lazy 200-file open:** Bulk opens (`open_many_lazy`) register lightweight `LazyDocumentStub`
   entries holding canonical path, OS `FileIdentity` (`(dev, ino)` on Unix, `(volume_serial,
   file_id)` on Windows), and open options without holding open file handles or eagerly indexing
   all 200 files. Only the active tab(s) hydrate immediately; indexing still runs through the
   shared `EnginePool`.
3. **Clean tab dormancy:** Clean idle tabs cancel read workers and drop `Document`, `Session`,
   `StructuralIndex`, row buffers, filter indexes, and whitespace caches while retaining
   `canonical_path`, `FileIdentity`, `SourceGuard`, view/selection/column state, and any active
   filter query. Hard-link/canonical-path deduplication and output path reservations remain
   authoritative across active, dormant, and stub tabs.
4. **Dirty tab dormancy & work preservation:** Dirty tabs retain sparse `cell_edits`,
   `header_renames`, `EditHistory`, and `WorkingCopyState` (keeping private working copies and
   Undo/Redo directories alive and reserved against scratch cleanup or overwrite) while evicting
   the heavy `StructuralIndex` and buffers and re-indexing on wake (`WakeKind::DirtyRehydrated`).
   An explicit sparse-overlay memory cap (`DEFAULT_MAX_DIRTY_OVERLAY_BYTES = 256 MiB`) and
   scratch-space headroom check (`MIN_DORMANT_SCRATCH_HEADROOM_BYTES = 16 MiB`) gate dirty
   dormancy. If all hydrated tabs are pinned, overlay memory hits its cap, or scratch space is
   exhausted, the scheduler returns an actionable `SchedulerError` (`AllTabsPinned`,
   `OverlayBudgetExceeded`, `ScratchDiskFull`) and **never silently drops edits or open tabs**.
5. **Active filters:** On dormancy, filter workers and filtered read jobs cancel cleanly,
   `FilterIndex` and `filtered_rows` are evicted, and the `FilterQuery` plus source-row anchor
   are retained. Waking rebuilds matches deterministically against the same generation
   (`WakeKind::FilteredRebuilding`; `< 50 ms` warm latency is never promised for filtered or
   dirty rehydration).
6. **Compressed (`.gz`) phase rules (`CompressedDormancyPolicy`):**
   - `Phase::Pending` and `Phase::Decompressing` are pinned (never demoted before storage
     decision or while decompression is running).
   - `Phase::Complete` demotes to `DormantCompressedComplete`, evicting `StructuralIndex` and
     buffers while keeping the private decompressed copy alive and reserved; waking re-indexes
     that private copy without re-decompressing.
   - `Phase::Preview` demotes to `DormantCompressedPreview` and wakes directly into preview mode
     without ever silently restarting decompression.
7. **External source changes while dormant:** Every wake validates its `SourceGuard` against
   the source path on disk. Any external modification or removal surfaces
   `WakeOutcome::SourceChangedConflict` (`source_changed = true` and `SOURCE_CHANGED_NOTICE`)
   instead of silently reloading.
8. **`RLIMIT_NOFILE` raise & code-counted `< 128` FD budget:** At startup, Quarry calls
   `quarry_core::raise_nofile_limit(4096)` toward the hard limit without lowering a higher
   existing soft limit. Internally, `FdBudgetLedger` tracks explicit RAII `FdLease` reservations
   across `ActiveDocument`, `UndoRedo`, `SidecarCache`, `TempDirectory`, `GuiSystem`,
   `CleanupWorker`, and `BackgroundJob` under a strict `< 128` cap (`MAX_PROCESS_FD_BUDGET`).
9. **Quit semantics preserved:** `cancel_quit` retains every active, dormant, dirty, and stub
   tab along with all working copies and Undo/Redo directories; `finalize_quit` shuts down and
   drops every tab, removing all Quarry-owned private working files with zero orphans.
10. **Write-job cap retained:** `MAX_WRITE_JOBS = 2` remains unchanged unless a later benchmark
    explicitly justifies raising it.
11. **Parent/section dirty mutual exclusion and post-splice refresh (`2026-11-24`, [ADR 0015](0015-guarded-section-splice-save.md)):**
    Across all windows and hard-link aliases (hydrated and dormant), at most one tab for a given
    underlying file (parent or section) may hold unsaved edits at a time. Guarded section
    splice-save reserves the parent path in `write_destination`, reloads the spliced section at its
    updated `Verified` byte span, and marks clean sibling/parent tabs `source_changed = true` with
    `SOURCE_CHANGED_NOTICE` rather than silently reloading.

### Verification

- Deterministic 200-file synthetic corpus generator: `scripts/generate-200-file-corpus.py`.
- Headless workbench integration suite: `crates/quarry-workbench/tests/dormant_tabs.rs`
  (covering 200-file lazy open with `< 128` code-counted and OS open FDs, hard-link
  deduplication while dormant/stub, dirty dormancy + Undo/Redo protection + overlay/disk-full
  preservation, all-pinned preservation, filtered wake rebuild, `.gz` phase rules, external
  source-change conflict, and cancelled vs completed quit cleanup).
