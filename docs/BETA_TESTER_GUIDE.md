# Quarry beta tester guide

**Public beta:** use disposable copies and report problems with reproducible examples.

This beta targets Apple Silicon Macs on macOS 26 and 27 and is available free
from GitHub Releases. Broader acceptance remains in progress; the evidence
below identifies what has passed. Intel Macs, Linux desktop and Windows are outside scope.

## Candidate and compatibility

- Current release: **0.1.0 beta 2, build 140**, clean revision
  `fc0a0b8b59278c96ad69756e0e20b2bffed7001d`.
- Tested: **macOS 27.0.1 (26A434), Apple Silicon**. A VMPal update from build
  127 passed signature/Gatekeeper checks, launch, grid lines, auto-fit and
  long-cell display/copy, editing and Undo/Redo, unsaved-quit protection, exact
  Save As/reopen, and About build details.
- macOS 26 validation remains outstanding for this archive. Browser download
  with quarantine, clean-environment first launch and offline first launch
  are each unverified for build 140. Beta 1 results do not count as beta 2 passes.
- Complete connected workflows, wide-grid accessibility, large-file GUI and
  temporary-storage recovery remain open for this archive. The previous CJK
  font-rendering limitation has not been retested.
- Archive: `Quarry-0.1.0-beta.2-macos-arm64.zip`, **3,629,137 bytes**, signed,
  notarized and stapled.
- SHA-256: `4b7ef0e0afbf0c4724b1a85d748562038b61b011cd047e798183898db9335bc6`.
- Read the [release notes](releases/v0.1.0-beta.2.md) and
  [build 140 verification report](benchmarks/2026-10-06-beta-build140.md).

## Install or update

1. Save any open work and quit Quarry completely before replacing the app.
   Keep a copy of your previous app until the new copy works.
2. Download the ZIP from
   [GitHub Releases](https://github.com/danchamorro/quarry/releases/tag/v0.1.0-beta.2). To check a copy in Downloads, run
   `shasum -a 256 ~/Downloads/Quarry-0.1.0-beta.2-macos-arm64.zip` in Terminal and compare the
   result with the SHA-256 above. Report a mismatch before proceeding.
3. Extract the ZIP and move `Quarry.app` into Applications.
4. Open Quarry normally from Applications. Record any macOS first-launch
   message. If macOS blocks launch, stop and report the exact message; do not
   bypass Gatekeeper or change security settings.

Record your macOS version and Mac model from **Apple menu → About This Mac**,
whether this was a first installation or update, and whether launch succeeded.

## What to test

Use synthetic data or disposable copies. Keep an untouched original, record
its SHA-256 before testing, and use **Save As** with a new name for edits.
Confirm the original hash remains unchanged after operations that preserve it.
See the [user guide](USER_GUIDE.md) for each control's behavior.

1. Open a CSV through the app and Finder. Check columns, row counts, quoted
   commas, multiline values, blank cells, and Unicode.
2. Edit a cell and header, then Undo and Redo each change. Save As, quit,
   reopen the new file, and confirm its exact values and unchanged original.
3. Search, hide, reorder, reset, and auto-fit columns. Check picker search,
   keyboard focus, Escape, and a smaller window.
4. Apply text and numeric **Between** rules on different columns. Check both
   endpoints, invalid bounds, Clear, and exported rows against expected data.
5. Clear filters, sort text and numbers, and check order and header retention.
   Find duplicates, review counts, remove extras, then test Undo and Redo.
6. With a larger disposable file, check progress and cancellation. Confirm the
   source remains intact, cancelled exports are absent, and editing still works.
   Check **Temporary storage…** before operations that need working space.

## Feedback and limits

Report failures with the [GitHub bug form](https://github.com/danchamorro/quarry/issues/new?template=bug_report.yml).
Open **File menu → About & Feedback…** and choose **Copy build details**;
the same panel links to the feedback form and user guide.
Include candidate version, macOS/model, steps, expected and actual results,
file size, and whether the source or unsaved work was affected. For slow jobs,
include elapsed time, memory, available disk, and the displayed operation phase.
GitHub reports are public: redact paths, screenshots, logs, and values. Share
only synthetic reproductions, never private datasets or credentials.

Clear active filters before editing; save or discard cell edits before filtering.
Undo history is bounded, and case-insensitive matching folds ASCII letters only.
Read the [documented limits](USER_GUIDE.md#undo-and-redo-changes) and
[filter rules](USER_GUIDE.md#filter-rows). CSV append, document tabs, and a TUI
remain [future work](ROADMAP.md#post-beta-sequence). The
[beta checklist](BETA_RELEASE_CHECKLIST.md) tracks acceptance still outstanding.
