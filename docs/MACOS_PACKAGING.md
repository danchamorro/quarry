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

The bundle declares Quarry as an editor for macOS CSV documents. Finder,
**Open With**, and a default-app association therefore deliver CSV file-open
requests to the running application instead of rejecting the format.

The package records the Cargo version, the full-history Git commit count as its
numeric build version, the full Git commit, the built architecture, and whether
the source tree was clean. A release candidate is acceptable only when
`QuarrySourceStatus` is `clean` and `QuarryGitRevision` equals the intended
commit.

The [beta release checklist](BETA_RELEASE_CHECKLIST.md) separates local package
validation from the acceptance and distribution gates for a public candidate.

The plist declares macOS 11.0 as its minimum. The packaging command does not pin
or verify the Mach-O deployment target, and the current acceptance run exercises
only the documented Apple Silicon host.

## Prerequisites

- macOS with the Xcode command line tools.
- The Rust toolchain selected by `rust-toolchain.toml`.
- Python 3 for the offline notice freshness check.
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
icon and license notices, applies an ad-hoc signature without a timestamp, and
verifies the bundle.
It rechecks the Git revision and working state after compilation and stops if
either changed during the build.
Two consecutive runs from the same checkout, pinned Rust 1.88.0 toolchain, and
macOS SDK produced identical plist, executable, and icon payload hashes. The
runner-provided macOS SDK is not pinned, so this is not a claim of byte-identical
output across machines or SDK updates.

Verify a packaged candidate again with:

```bash
./scripts/macos-app.sh verify target/package/Quarry.app
```

## Install and update

```bash
./scripts/macos-app.sh install
```

The installer:

1. Acquires the exclusive per-user package lock and the application installation
   lock that every current Quarry process holds in shared mode.
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
Quarry also fails closed when launched during installation, before opening a
window or reading an input file. Process-name checks remain for legacy builds
that do not yet participate in the installation lock.
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
and executable, nonempty version and source metadata, packaged icon, and
nonempty license-notice resources. Release
acceptance additionally requires the printed source status to be `clean` and
the printed app revision to equal `git rev-parse HEAD`; those two comparisons
remain explicit manual gates.

## License resources

Local packages include these files in `Contents/Resources/Licenses/`:

- `LICENSE-MIT` and `LICENSE-APACHE`, copied unchanged from the repository.
- `THIRD_PARTY_NOTICES.html`, generated from the locked macOS dependencies and
  the additional font notices recorded in the license audit.

Before packaging, the notice freshness check must pass:

```bash
./scripts/generate-notices.sh --check
```

This check establishes that the checked-in notice artifact matches its recorded
inputs. It does not establish that every attribution question has been resolved.
The current third-party artifact is a draft; its unresolved audit items remain
a beta release gate. Follow the audit and regeneration instructions in
[the license audit](../packaging/licenses/AUDIT.md) when dependencies or bundled
assets change. Do not replace missing upstream copyright information with
invented attribution or silently accept generic template text as complete.

Normal packaging uses the checked-in artifact and does not install a license
tool or download notice text. The installer still verifies older applications
as rollback sources using their identity and signature; older packages may lack
these new resources and will not pass the expanded current `verify` command.
The draft inventory currently covers `aarch64-apple-darwin`. Packaging checks
that it matches the native build target; other architectures need their own
reviewed inventory before packaging can proceed.

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

On 2026-09-07 the local host reported zero valid code-signing identities, while
`notarytool` was available. The clean installed application at `69d5f15` was
validated on macOS 26.6.2, Apple Silicon. Its Mach-O build command declares a
minimum of macOS 11.0 and SDK 26.5; this does not prove runtime compatibility
with macOS 11.0. Select supported systems and test the exact release candidate
before promising a minimum OS version.

Apple's current [notarization guidance](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
requires the appropriate Developer ID signature and hardened runtime for that
distribution workflow. Signing, notarization submission, stapling, and
Gatekeeper acceptance have not been validated for a beta candidate. Keep those
gates open until the signing identity is available and the exact candidate has
passed them.
