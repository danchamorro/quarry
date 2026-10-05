# Quarry invited tester guide

**Preparation draft:** remaining acceptance and owner approval precede invitations.

This beta targets Apple Silicon Macs on macOS 26 and 27. GitHub Releases is
the selected free download channel. Acceptance on both versions must finish
before publication. Intel Macs, Linux desktop and Windows are outside scope.

## Candidate and compatibility

- Candidate: **0.1.0 (127)**, clean revision
  `30fee5d62ff23bb4803f73fbe5b14129162efb43`.
- Host tested: **macOS 27.0.1 (26A434), Apple Silicon**. Bounded signed-ZIP
  workflows passed; this does not yet establish full support on macOS 27.
- macOS 26.6.2 file install/rollback, Gatekeeper and native launch passed; its
  complete native workflow and fresh-environment acceptance remain open.
- Archive: `Quarry-0.1.0-beta.1-macos-arm64.zip`, **3,622,188 bytes**, signed and notarized.
- SHA-256: `f8fac678f75343bd5eef856826a274ed965928881a8aa02b04dab7452af3cc44`.
- Read the [draft release notes](BETA_RELEASE_NOTES.md) and
  [acceptance evidence](benchmarks/2026-10-05-beta-build127.md).

## Install or update

1. Save any open work and quit Quarry completely before replacing the app.
   Keep a copy of your previous app until the new copy works.
2. Once published, download the ZIP from
   [GitHub Releases](https://github.com/danchamorro/quarry/releases). To check a copy in Downloads, run
   `shasum -a 256 ~/Downloads/Quarry-0.1.0-beta.1-macos-arm64.zip` in Terminal and compare the
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
