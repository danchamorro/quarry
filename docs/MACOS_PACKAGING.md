# macOS packaging and installation

## Scope

Phase 7A packages the selected egui desktop into one canonical local alpha
application:

| Property | Value |
|---|---|
| Product | Quarry |
| Installed path | `/Applications/Quarry.app` |
| Bundle identifier | `io.github.danchamorro.quarry` |
| Executable | `Quarry` |
| Info.plist minimum declaration | 11.0 |
| Current validated architecture | Apple Silicon (`arm64`) |
| Icon source | `assets/quarry-logo-v3.png` |

The package records the Cargo version, the full-history Git commit count as its
numeric build version, the full Git commit, the built architecture, and whether
the source tree was clean. A release candidate is acceptable only when
`QuarrySourceStatus` is `clean` and `QuarryGitRevision` equals the intended
commit.

The plist declares macOS 11.0 as its minimum. The packaging command does not pin
or verify the Mach-O deployment target, and the current acceptance run exercises
only the documented Apple Silicon host.

## Prerequisites

- macOS with the Xcode command line tools.
- The Rust toolchain selected by `rust-toolchain.toml`.
- A non-shallow Quarry checkout with complete Git history and `Cargo.lock`
  present.
- Quarry, including a Cargo-launched development copy, must be closed before
  installation or update.

## Build and package

From the repository root:

```bash
./scripts/macos-app.sh package
```

The command performs a locked release build, creates
`target/package/Quarry.app`, adds versioned plist metadata and the checked-in
icon, applies an ad-hoc signature without a timestamp, and verifies the bundle.
It rechecks the Git revision and working state after compilation and stops if
either changed during the build.
Two consecutive runs from the same checkout, Rust toolchain, and macOS SDK
produced identical plist, executable, and icon payload hashes. Because the Rust
channel and macOS SDK are not pinned by this workflow, this is not a claim of
byte-identical output across machines or toolchain updates.

Verify a packaged candidate again with:

```bash
./scripts/macos-app.sh verify target/package/Quarry.app
```

## Install and update

```bash
./scripts/macos-app.sh install
```

The installer:

1. Acquires the exclusive per-user package and installation lock.
2. Refuses to continue while Quarry or the legacy prototype is running.
3. Builds and verifies a fresh candidate.
4. If a current app exists, saves it as a verified rollback archive.
5. Copies and verifies a candidate beside the final destination.
6. Replaces `/Applications/Quarry.app`, restoring the prior app if replacement
   or verification fails.
7. Confirms that the installed plist and signed executable exactly match the
   candidate.
8. Removes the legacy `/Applications/Quarry Egui.app` and the staged candidate,
   leaving only the canonical installed identity active.

Concurrent package or install commands from the same user fail without touching
shared candidates, backups, or the installed application.
An existing canonical or legacy bundle that fails validation is also left
untouched, and the update stops.

When a prior canonical app exists, its rollback archive is:

```text
~/Library/Application Support/Quarry/Backups/Quarry-previous.zip
```

When present, the first migration also preserves the old prototype as
`Quarry-Egui-legacy.zip` in that directory.

## Verify the installed app

```bash
./scripts/macos-app.sh verify
plutil -p /Applications/Quarry.app/Contents/Info.plist
plutil -extract QuarrySourceStatus raw /Applications/Quarry.app/Contents/Info.plist
plutil -extract QuarryGitRevision raw /Applications/Quarry.app/Contents/Info.plist
git rev-parse HEAD
open /Applications/Quarry.app
```

The verify command checks the strict code signature, canonical bundle identifier
and executable, nonempty version and source metadata, and packaged icon. Release
acceptance additionally requires the printed source status to be `clean` and
the printed app revision to equal `git rev-parse HEAD`; those two comparisons
remain explicit manual gates.

## Packaged-app smoke test

Use a disposable CSV outside Documents to avoid unrelated privacy prompts.
Launch `/Applications/Quarry.app`, then:

1. Open the disposable CSV.
2. Edit one existing cell directly in the grid.
3. Sort one selected numbered column.
4. Save As to a new path.
5. Verify the source bytes are unchanged and the new file has the exact edit
   and order.
6. Quit Quarry.
7. Relaunch `/Applications/Quarry.app` and reopen the saved file.
8. Confirm the file is clean and retains the edit and sort order.

The initial evidence is recorded in the
[Phase 7A packaged-app validation](benchmarks/2026-08-21-packaged-app.md).

## Rollback

For the most controlled rollback, install a known-good commit from a separate
worktree. This keeps the current development checkout untouched:

```bash
git worktree add ../quarry-rollback <known-good-commit>
../quarry-rollback/scripts/macos-app.sh install
git worktree remove ../quarry-rollback
```

For an immediate local rollback, quit Quarry, expand `Quarry-previous.zip`,
replace `/Applications/Quarry.app` with the archived `Quarry.app` in Finder,
then run `./scripts/macos-app.sh verify`.

## Signing limitation

The current alpha uses an ad-hoc signature because no Developer ID Application
identity is installed. The signature detects post-signing bundle changes, but
it does not identify a trusted publisher, provide a Team ID, prove Gatekeeper
distribution readiness, or support notarization. A rebuilt ad-hoc app may also
be treated as a new identity by macOS privacy controls.

Public distribution requires a Developer ID Application certificate, stable
entitlements, hardened runtime, timestamping, notarization, stapling, and a
Gatekeeper assessment. Those steps remain deferred until the required Apple
Developer identity and credentials are available.
