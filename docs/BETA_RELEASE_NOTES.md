# Quarry 0.1.0 beta 1

**Public beta — broader validation is still in progress.**

Quarry is a free, open-source desktop tool for working with delimited files
larger than memory. This first beta targets Apple Silicon Macs running macOS
26 or 27. Validation is strongest on macOS 27.0.1; macOS 26.6.2 has passed
installation, rollback, signature checks and launch, with workflow testing incomplete.
Intel Macs, Linux desktop and Windows are outside this beta's scope.

## Included

- Progressive CSV opening and navigation, including wide files.
- Cell and header editing, Undo/Redo, Save and Save As.
- Column selection, visibility, order, auto-fit, split and combine.
- Text and numeric filters, inclusive Between rules and filtered export.
- Stable text/numeric sorting, character/word counts, shuffle and reverse.
- Duplicate review with explicit keep-first removal and selected-row deletion.
- Temporary-storage estimates, cancellation and recovery safeguards.
- Unsaved-work protection for the close button, Command-Q and menu Quit.

All features are available in source builds. Official downloads are free, with
no activation, paid edition, telemetry or automatic updater. The project uses
MIT OR Apache-2.0 licensing; the app includes third-party notices.

## Candidate identity

- App version/build: **0.1.0 (127)**, ARM64.
- Source: `30fee5d62ff23bb4803f73fbe5b14129162efb43`, clean.
- Archive: `Quarry-0.1.0-beta.1-macos-arm64.zip`, 3,622,188 bytes.
- SHA-256: `f8fac678f75343bd5eef856826a274ed965928881a8aa02b04dab7452af3cc44`.
- Developer ID signed, hardened runtime enabled, notarized and stapled.
- Distribution: [GitHub Releases](https://github.com/danchamorro/quarry/releases/tag/v0.1.0-beta.1).

## Known limits and validation still in progress

Use disposable copies during beta testing. Clear active filters before editing;
save or discard cell edits before filtering. Undo history is bounded, and
case-insensitive matching folds ASCII letters only. CSV append, document tabs
and a TUI are planned later.

Screen-reader and wide-grid accessibility validation is incomplete. Automation
can lose the content tree on larger grids, although visible grid rendering
has been observed. Compact-window, Finder/drag-and-drop, temporary-storage
recovery, complete macOS 26 workflow and fresh-environment browser-download
checks also remain open. This prerelease is available for testing with those
limits; it is not a stable-release compatibility guarantee.

On macOS 27.0.1, the signed app passed cell/header editing and Undo/Redo,
unsaved-quit protection, Save As/reopen, filtering, sorting and duplicate removal.
A 12 GB CSV also passed progressive opening, last-row navigation, filtering,
exact export comparison and export cancellation without changing the source.

## Install, update and feedback

Download the [signed archive](https://github.com/danchamorro/quarry/releases/download/v0.1.0-beta.1/Quarry-0.1.0-beta.1-macos-arm64.zip) and verify
its hash. Quit Quarry after saving your work, retain the previous app for
rollback, extract the ZIP and move Quarry.app to Applications. Open it normally;
report macOS blocking messages without bypassing Gatekeeper. To roll back,
quit Quarry and restore the previous app. See the
[tester guide](BETA_TESTER_GUIDE.md) for detailed checks.

Report bugs using the [GitHub issue form](https://github.com/danchamorro/quarry/issues/new?template=bug_report.yml).
Include app/build, exact macOS version, steps, expected/actual behavior and a
synthetic reproduction. Remove private data from public reports. Community
support is best effort, with no promised response time.
