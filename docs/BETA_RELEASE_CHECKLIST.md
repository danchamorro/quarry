# Beta release checklist

**Status, 2026-10-05:** clean **0.1.0 (127)** at
`30fee5d62ff23bb4803f73fbe5b14129162efb43` is the current signed candidate.
Signing, notarization, stapling, local Gatekeeper, extracted-bundle verification,
bounded native workflows, and 12 GB GUI and CLI/core checks passed. The beta
is published as [v0.1.0-beta.1](https://github.com/danchamorro/quarry/releases/tag/v0.1.0-beta.1).
Broader acceptance remains incomplete; follow-up checks are listed below.

The owner selected **GitHub Releases** for free downloads and **Apple Silicon
on macOS 26 and 27**. Publication was subsequently authorized as a beta with
the current validation limits documented. Full acceptance on both remains
follow-up work, not a claim that every version in those families has passed.
Keep app bundles out of Git source history; upload approved archives as release
assets. The main application remains egui.

See the [build 127 evidence](benchmarks/2026-10-05-beta-build127.md),
[tester guide](BETA_TESTER_GUIDE.md), [release notes](BETA_RELEASE_NOTES.md),
and [packaging procedure](MACOS_PACKAGING.md).
[Earlier candidate evidence](BETA_RELEASE_HISTORY.md) is preserved separately;
the [September desktop report](benchmarks/2026-09-20-desktop-build118.md)
does not count as an exact-build-127 run.

## Platform matrix

| Platform | Build 127 evidence | Remaining acceptance |
|---|---|---|
| macOS 27, Apple Silicon | Host 27.0.1 (26A434): signed ZIP verified, native small workflows, 12 GB GUI open/filter/export/cancellation and CLI/core checks passed | Wide-grid accessibility, complete compact-window and Finder/drag-and-drop workflows, temporary-storage recovery, fresh-environment first launch |
| macOS 26, Apple Silicon | 26.6.2 (25G83): signature, staple, Gatekeeper, file installation/rollback and native launch passed | Complete connected native workflow and fresh-environment first launch |
| Intel Mac, Linux desktop, Windows | No release acceptance | Outside this beta's scope |
| Linux core/CLI | CI checks pass for the packaging-fix source revision | Engineering evidence only; not desktop support |

The bundle and Mach-O minimum declaration of macOS 11.0 is not a support claim.
Test and record exact OS versions; do not infer compatibility across an entire
major version from a single run.

## Release record and remaining acceptance

Repository integration and required CI are tracked in
[PR #52](https://github.com/danchamorro/quarry/pull/52). Publication approval
does not mark the unchecked validation items below as passed.

- [x] Complete pre-beta feature priorities; see the [feature checklist](PRE_BETA_CHECKLIST.md).
- [x] Freeze version, clean source revision, build identity, toolchain and archive hash.
- [x] Pass formatting, strict Clippy, 315 workspace tests, locked release build,
  16 notice tests, installer self-tests, packaging and bundle verification.
- [x] Regenerate/review notices. The packaging fix changes workspace discovery;
  dependency versions and notice HTML are unchanged.
- [x] Sign with Developer ID and hardened runtime, notarize, staple, assess local
  Gatekeeper, and verify every extracted bundle file and staple.
- [x] Verify native cell/header Undo/Redo, unsaved Command-Q protection, exact
  Save As and reopen, combined filtering/export, invalid bounds, numeric sort,
  duplicate review/removal, and basic column controls on the signed ZIP.
- [x] Run a current 12 GB CLI/core export and cancellation check, compare every
  exported record with an independent oracle, and verify the source hash.
- [x] Select free download host and target OS/architecture scope.
- [x] Create an unpublished GitHub draft with the exact signed archive and checksum.
- [ ] Complete the connected native workflow on both target macOS versions,
  including wide files, accessibility, smaller windows, Finder opening,
  drag-and-drop, and temporary-storage recovery.
- [x] Validate GUI progressive open/navigation, full-file filtering/export,
  cancellation and sampled memory on the 12 GB public fixture; verify every
  exported record and unchanged source hash.
- [ ] Complete GUI temporary-storage checks, including peak disk use,
  selected-folder sort/Undo retention and insufficient-space recovery.
- [x] Verify file installation, update and rollback for this exact signed candidate
  in the macOS 26 VM; native Finder installation/download remains separate.
- [ ] Verify browser-downloaded, quarantined first launch on a fresh supported
  Mac or clean VM. The previously used VM alone does not close this gate.
- [ ] Resolve defects found during the remaining acceptance checks and record
  applicable workarounds in the release notes.
- [x] Obtain authorization to publish this beta with the documented validation limits.
- [x] Publish the approved prerelease and verify its download/hash.

## Connected acceptance workflow

Use disposable synthetic files containing quoted commas, embedded newlines,
Unicode, blanks, missing fields, duplicate values and numeric boundaries.
Record original hashes and compare exact outputs, not only displayed counts.
Repeat column/navigation checks with more than 64 columns.

1. Open through Finder and the app; check format detection, progressive rows,
   navigation, source-column numbers and cancellation.
2. Edit a cell and header, Undo/Redo, and Save As; verify multiline content,
   exact output and unchanged original. Quit and reopen the output.
3. Search, hide, reorder, reset and auto-fit columns. Verify persistence,
   keyboard focus, Escape and a compact window.
4. Combine text and inclusive numeric Between rules. Check search, invalid
   bounds, preserved active results, Clear and exact filtered export.
5. Verify sort modes, stable ties, header retention, cancellation and Undo/Redo.
6. Review duplicate counts, cancel, then remove extras. Verify keep-first
   behavior, exact saved output and Undo/Redo.
7. Exercise selected scratch storage and insufficient-space recovery. Check
   cleanup after cancellation and retention of files still needed for Undo.

Record revision, OS/architecture, fixture, expected/actual result, source
preservation and limitations for every run. A later documentation commit does
not change the frozen signed archive. Any rebuilt application is a new
candidate and must receive its own identity and required acceptance.
