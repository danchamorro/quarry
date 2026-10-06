# Beta release checklist

**Public release status, 2026-10-06:** [v0.1.0-beta.2](https://github.com/danchamorro/quarry/releases/tag/v0.1.0-beta.2)
is published with clean **0.1.0 (140)** at
`fc0a0b8b59278c96ad69756e0e20b2bffed7001d`. The exact signed archive and checksum
passed an anonymous public download check. See its
[verification report](benchmarks/2026-10-06-beta-build140.md) and
[release notes](releases/v0.1.0-beta.2.md).

## Current beta 2 acceptance

The following applies only to build 140. Publication with documented validation
limits does not mark the unchecked items as passed.

- [x] Freeze the clean revision, build, toolchain and archive SHA-256.
- [x] Verify Developer ID signature, hardened runtime, notarization, staple,
  Gatekeeper and the extracted app against the frozen bundle.
- [x] Verify a VMPal update from build 127 on macOS 27.0.1 (26A434), retaining
  a signature-checked rollback copy.
- [x] Verify launch, cell grid lines, auto-fit and long-value display/copy,
  cell edit/Undo/Redo, unsaved quit, exact Save As/reopen and About details.
- [x] Publish the approved prerelease and verify tag, public ZIP size and checksum.
- [ ] Validate this archive on macOS 26.
- [ ] Complete connected native workflows, wide-grid accessibility, large-file
  GUI and temporary-storage recovery checks for this archive.
- [ ] Verify first launch after a browser download with quarantine.
- [ ] Verify first launch in a clean environment and first launch while offline.
- [ ] Retest the earlier CJK font-rendering limitation.

## Distribution policy

The owner selected **GitHub Releases** for free downloads and **Apple Silicon
on macOS 26 and 27**. Publication was subsequently authorized as a beta with
the current validation limits documented. Full acceptance on both remains
follow-up work, not a claim that every version in those families has passed.
Keep app bundles out of Git source history; upload approved archives as release
assets. The main application remains egui.

## Historical beta 1 acceptance

Everything in the next two subsections applies only to **build 127**, not the
current build 140. The older archive remains available as
[v0.1.0-beta.1](https://github.com/danchamorro/quarry/releases/tag/v0.1.0-beta.1).

See the [build 127 evidence](benchmarks/2026-10-05-beta-build127.md),
[VMPal installation report](benchmarks/2026-10-06-vmpal-beta-install.md), and
[beta 1 release notes](releases/v0.1.0-beta.1.md).
[Earlier candidate evidence](BETA_RELEASE_HISTORY.md) is preserved separately;
the [September desktop report](benchmarks/2026-09-20-desktop-build118.md)
does not count as an exact-build-127 run.

### Build 127 platform matrix

| Platform | Build 127 evidence | Remaining acceptance |
|---|---|---|
| macOS 27, Apple Silicon | Host 27.0.1 (26A434): signed ZIP verified, native small workflows, 12 GB GUI open/filter/export/cancellation and CLI/core checks passed. VMPal guest on the same OS: Chrome download with quarantine, Finder installation, normal first launch and basic CSV opening passed | Wide-grid accessibility, complete compact-window and Finder/drag-and-drop workflows, temporary-storage recovery, fresh-environment first launch |
| macOS 26, Apple Silicon | 26.6.2 (25G83): signature, staple, Gatekeeper, file installation/rollback and native launch passed | Complete connected native workflow and fresh-environment first launch |
| Intel Mac, Linux desktop, Windows | No release acceptance | Outside this beta's scope |
| Linux core/CLI | CI checks pass for the packaging-fix source revision | Engineering evidence only; not desktop support |

The bundle and Mach-O minimum declaration of macOS 11.0 is not a support claim.
Test and record exact OS versions; do not infer compatibility across an entire
major version from a single run.

### Build 127 release record and incomplete checks

Repository integration and required CI are tracked in
[PR #52](https://github.com/danchamorro/quarry/pull/52). Publication approval
does not mark the unchecked validation items below as passed.

- [x] Complete pre-beta feature priorities; see the [feature checklist](PRE_BETA_CHECKLIST.md).
- [x] Freeze version, clean source revision, build identity, toolchain and archive hash.
- [x] Pass formatting, strict Clippy, 315 workspace tests, locked release build,
  17 notice tests, installer self-tests, packaging and bundle verification.
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
- [x] Verify browser download with quarantine, Finder installation and normal
  first launch in the macOS 27.0.1 VMPal guest, with no prior app installation
  at `/Applications/Quarry.app`.
- [ ] Verify browser-downloaded, quarantined first launch on a fresh supported
  Mac or clean VM. Neither existing VM establishes a newly provisioned
  environment or absence of cached Gatekeeper assessments; the VMPal run
  does not close this gate.
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
