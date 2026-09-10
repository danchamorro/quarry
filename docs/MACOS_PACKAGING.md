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
The current frozen candidate is [0.1.0 (105), clean `159e149`](#clean-local-candidate-2026-09-08).
The earlier trial and build 98 records below remain historical evidence.

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
- `Rust/`, containing seven runtime copyright/license resources from the
  inspected Rust 1.88.0 toolchain and its pinned sources. The
  [audit's runtime table](../packaging/licenses/AUDIT.md#rust-runtime-resources)
  records their provenance, including backtrace and compiler-builtins notices.

Before packaging, the notice freshness check must pass:

```bash
./scripts/generate-notices.sh --check
```

This check establishes that the checked-in notice artifact matches its recorded
inputs. It does not establish that every attribution question has been resolved.
The locked ARM64/Rust 1.88.0 inventory has a recorded source review.
`./scripts/generate-notices.sh --release-check` checks freshness and requires
the exact manifest hash recorded in `packaging/licenses/reviewed.sha256`.
Regeneration never renews this review record. This is an inventory gate,
separate from candidate validation and release authorization.
Follow the audit and regeneration instructions in
[the license audit](../packaging/licenses/AUDIT.md) when dependencies or bundled
assets change. Do not replace missing upstream copyright information with
invented attribution or silently accept generic template text as complete.

Normal packaging uses the checked-in artifact and does not install a license
tool or download notice text. The installer still verifies older applications
as rollback sources using their identity and signature; older packages may lack
these new resources and will not pass the expanded current `verify` command.
The inventory currently covers `aarch64-apple-darwin`. Packaging checks
that it matches the native build target; other architectures need their own
reviewed inventory before packaging can proceed. The same guard checks the
compiler release against the Rust runtime notices. Compiler discovery runs in
the build checkout and honors `RUSTC`; a different release requires a new audit.

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
then check its signature and recorded identity:

```bash
codesign --verify --deep --strict --verbose=2 /Applications/Quarry.app
plutil -p /Applications/Quarry.app/Contents/Info.plist
```

Confirm `CFBundleIdentifier` is `io.github.danchamorro.quarry`,
`CFBundleExecutable` is `Quarry`, and the recorded revision and source status
match the intended backup. These checks also work for older bundles without
license resources. Run `./scripts/macos-app.sh verify` as an additional check
only when the restored bundle includes all current required license resources;
its stricter current-package contract still applies to new builds.

## Signing limitation

The packaging script still produces an ad-hoc signature. The signature detects
post-signing bundle changes, but it does not identify a trusted publisher,
provide a Team ID, prove Gatekeeper
distribution readiness, or support notarization. A rebuilt ad-hoc app may also
be treated as a new identity by macOS privacy controls.

Public distribution requires a Developer ID Application certificate, stable
entitlements, hardened runtime, timestamping, notarization, stapling, and a
Gatekeeper assessment. Developer ID signing is currently a separate manual
step; the script does not submit or staple packages.

At preparation start on 2026-09-07 the local host reported zero valid
code-signing identities, while `notarytool` was available. The clean installed
application at `69d5f15` was
validated on macOS 26.6.2, Apple Silicon. Its Mach-O build command declares a
minimum of macOS 11.0 and SDK 26.5; this does not prove runtime compatibility
with macOS 11.0. Select supported systems and test the exact release candidate
before promising a minimum OS version.

Apple's current [notarization guidance](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
requires the appropriate Developer ID signature and hardened runtime for that
distribution workflow. Signing, notarization submission, stapling, and
local Gatekeeper acceptance have now passed for both the trial and the clean
candidate below. Fresh-environment acceptance remains open for the exact clean
candidate; the earlier trial's VM results do not substitute for that check.

## Signing setup and local validation

Signing requires an Apple Developer Program account with access to create a
Developer ID Application certificate, and the corresponding certificate and
private key available on the signing Mac. A certificate without its private key
is not a signing identity. Apple's [Developer ID guide](https://developer.apple.com/developer-id/)
describes creating the certificate in Xcode or the developer account.

After that owner-controlled setup, confirm the usable identity and store
notarization credentials through the interactive Keychain prompt:

```bash
security find-identity -v -p codesigning
xcrun notarytool store-credentials "quarry-notary"
```

The installed `notarytool` accepts omitted credential options as secure
interactive prompts and validates them before storing them. Keep passwords and
private keys out of repository files and command arguments. This setup command
does not submit an app. During the subsequent owner setup, Xcode created a
usable Developer ID Application identity and the owner validated and stored
the `quarry-notary` profile in Keychain.

### Local notarization trial, 2026-09-07

The trial used a copy of the dirty `b461771e6fe8b78be398208a9830d8c3e2c041c7`
package at `target/quarry-signing-check.u6AvD7/Quarry.app`, version 0.1.0 (97),
ARM64, on macOS 26.6.2. Developer ID signing with `--options runtime --timestamp`
passed signature and bundled-resource verification. The signed app opened a
synthetic CSV through the native file picker, indexed two rows and three
columns, and rendered the expected values.

Apple accepted submission `bdabafff-652c-4350-985c-17b557134f56`, uploaded at
`2026-09-08T01:04:17.149Z` (September 7 locally), with no reported issues.
`stapler staple`, `stapler validate`, and local `spctl --assess --type execute`
passed; Gatekeeper reported `source=Notarized Developer ID`.

Both archives and `notarization-log.json` remain local in the trial directory:

| Archive | Bytes | SHA-256 |
|---|---:|---|
| Submitted `Quarry-notarization.zip` | 3,605,784 | `c46bc52a2be551a01e2afc99f0ef1bc4f66bf21fe4917b4b836673f1dff3b62c` |
| Repacked after stapling, `Quarry-notarized.zip` | 3,607,389 | `f40994f312890c86d218415d805aa0892ba682c55f4766836b39bb3f32e490ef` |

After extracting the repacked ZIP, bundle verification, ticket validation, and
Gatekeeper assessment also passed for the extracted app.

The owner then reported successful first launch inside a fresh Parallels macOS
VM after downloading this ZIP, and explicitly confirmed the VM's network was
disconnected before first launch. This is owner-reported offline launch evidence
for the dirty trial. Read-only guest commands confirmed macOS 26.6.2 (25G83),
ARM64, and the installed 0.1.0 (97) package at dirty `b461771`. No snapshot was
taken, at the owner's request.

The owner subsequently reported that all five requested functional checks
worked with the synthetic `quarry-test.csv`: open five records/six columns,
filter Active rows with Amount between 100 and 500 inclusive (three records),
remove the extra duplicate and undo back to five records, numeric ascending
sort (100, 500, 500, 1000, 1200), and edit/Save As/quit/reopen persistence.
These are owner-reported results; the computer-use attempt did not independently
complete the workflow. Guest file-content/hash verification was unavailable
through the CLI because guest privacy controls denied access to Downloads.

A fresh macOS VM is suitable for the clean-environment Gatekeeper check;
Apple's [testing guidance](https://developer.apple.com/forums/thread/130560)
recommends VMs for this purpose. Repeating a first-launch test requires a fresh
environment because Gatekeeper caches previous results. VM results do not
establish performance or compatibility on other physical hardware.

This proves the signing account and notarization workflow, including the
reported offline VM launch. The notice audit was reviewed separately afterward.
The trial does not validate the final clean release candidate or authorize publication. The
installed clean `b461771` host app was not replaced.

### Clean local candidate, 2026-09-07

This historical candidate is superseded by build 105 below. The following
record preserves the results and outstanding gates at the time it was frozen.

The frozen candidate in `target/quarry-beta-candidate.cbd9446/` is version
**0.1.0 (98)**, ARM64, from clean commit
`cbd9446fb34b3b08d46d9fa85a07d77d4a5256d5` on `codex/beta-license-audit`.
Its Cargo and bundle versions agree. It was built with Rust 1.88.0
(`6b00bc3880198600130e1cf62b8f8a93494488cc`), LLVM 20.1.5, and SDK 26.5.
The Mach-O minimum is macOS 11.0, which is not a supported-system claim.
The source executable SHA-256 before Developer ID signing is
`b8e2414aff6e6b5b0e705b128c98a7148ae152d4a5d4cac8f29ea682ba40bfb6`.

Formatting, strict Clippy, 296 workspace tests, the locked release build,
12 notice tests, packaging self-tests, and the notice release check passed.
All ten candidate license resources matched their reviewed files. The
third-party HTML is 222,830 bytes, SHA-256
`6a816f8e7da6ec6ff2fed36f8a666c1d6eeab062ed927aaf3915a8da5615a3e3`.
All four embedded fonts were verified; native links contain only Apple system
libraries and frameworks.

Developer ID signature and bundle verification passed. Apple accepted
submission `a2bd2114-03b8-4221-98bd-a785c2e9c2ae`, uploaded at
`2026-09-08T01:57:51.561Z` (September 7 locally), with no reported issues.
Staple validation and local Gatekeeper assessment passed, with Gatekeeper
reporting `Notarized Developer ID`. Files extracted from the final ZIP matched
the signed, stapled bundle.

| Archive | Bytes | SHA-256 |
|---|---:|---|
| Submitted `Quarry-notarization.zip` | 3,606,646 | `bf5a19ca2f16b9277a2a896e7f977997c1aa1767821b46f94b44635921c7212a` |
| Repacked after stapling, `Quarry-notarized.zip` | 3,608,251 | `13d0d467d76e394faa4243d1602d8a0d9d0afea40b78f6da1b8d2d02e75355f2` |

The signed candidate also passed a local native smoke test: it opened a
synthetic CSV through the native picker, indexed five records and six columns,
rendered the expected values including multiline values, and quit without
edits. The fixture hash was unchanged. The local `candidate-evidence.json`,
`build-evidence.json`, and `notarization-log.json` record these checks alongside
the frozen archives.

The owner subsequently reported "Tested on VM and working" for the requested
retest of the final clean candidate. This is owner-reported acceptance in the
existing macOS 26.6.2 ARM64 Parallels VM; no newly created environment or
independent verification of every workflow operation is claimed.

This clean candidate is separate from the dirty 0.1.0 (97) trial tested in
Parallels. It still needs fresh-environment acceptance, the final connected
workflow and large-file checks, supported OS/architecture decisions, and exact
candidate installation/update/rollback. The installed host app remains clean
`b461771`. Source changes are pushed in
[PR #42](https://github.com/danchamorro/quarry/pull/42), whose macOS/Linux CI at
`5d7fc88` passed. Review disposition and merge, release notes, and explicit
release authorization remain open. No package has been published.

This evidence is a documentation follow-up to the frozen build. A subsequent
documentation commit does not change the candidate's `cbd9446` revision,
clean-source metadata, build number, or archive hashes. Rebuilding from a
different commit creates a different candidate requiring its own evidence.

### Clean local candidate, 2026-09-08

After [PR #43](https://github.com/danchamorro/quarry/pull/43) merged, version
**0.1.0 (105)** was frozen at clean
`159e14950f6b4694129a810e19299c2f7fb950df`, source tree
`bace7d5898807191030309959e33852cc1c28750`. Local artifacts are under
`target/quarry-beta-candidate.159e149.q4a2Kw/`. This tree equals the validated
PR head `c37b797`: 303 macOS workspace tests, 184 Linux core/CLI/parser tests,
strict Clippy, formatting, locked releases, and packaging self-tests passed
before merge. The new clean package and bundle verification passed separately.
The build used Rust 1.88.0, LLVM 20.1.5, SDK 26.5, and ARM64 on macOS 26.6.2.
All ten license resources matched the reviewed files; notice HTML is unchanged.

The installer updated `/Applications/Quarry.app` to the same clean revision
and build and preserved its verified `Quarry-previous.zip` rollback backup.
The installed ad-hoc executable SHA-256 is
`de576c9745980dfc1c4bbc711c31d3e5eb65670402fe3e069d7896b7e20c1c37`.
Native picker opening, a controlled ctime-only xattr change, Undo/Redo without
reload, exact Save As with source preservation, quit, and native-picker reopen
passed. This installed-app result is separate from signed-candidate acceptance.

The candidate copy received a Developer ID signature. Apple accepted submission
`30a565de-89ff-44ed-8ad9-dd74309250d1`, uploaded at
`2026-09-08T14:19:52.640Z`, without issues. Signature verification, stapling,
ticket validation, and local Gatekeeper assessment passed. After extraction,
all bundle files matched, and bundle, ticket, and Gatekeeper checks passed.

| Archive | Bytes | SHA-256 |
|---|---:|---|
| Submitted `Quarry-notarization.zip` | 3,609,527 | `269cad89142d176bdfb1d0cec70e39ad7772e143927bd2a61677f538bd70dd21` |
| Repacked after stapling, `Quarry-notarized.zip` | 3,611,149 | `31e72cb0b8cafb62bca76e7d40b00ffcb6638fc10d5ca3100205e2ce8e7d80fb` |

The candidate directory records `candidate-evidence.json`, `build-evidence.json`,
`installed-evidence.json`, `installed-ui-evidence.json`, `candidate-ui-evidence.json`, and
`notarization-log.json`. Its `large-file/acceptance-summary.json` records a new
1 GB CLI/core numeric filter/export/cancellation run with independent exact-byte
verification and unchanged source. This is warm-cache engine evidence, not
packaged GUI or larger-than-RAM acceptance.

The signed app extracted from the final ZIP passed native picker opening,
a controlled xattr change, cell/header Undo/Redo, and exact Save As. Combined
numeric/text filtering and exact three-row export, numeric sorting with
Undo/Redo, and duplicate review Cancel/remove/Undo/Redo passed. Saved sort and
keep-first duplicate outputs matched exact expected bytes. Quit was confirmed
by process absence; native-picker reopen retained the edited header, cell,
and multiline value in a clean four-row/six-column file. Sources stayed intact.

The existing macOS 26.6.2 ARM64 Parallels VM received the ZIP through the local
guest channel, and its SHA-256 matched. The old clean build 98 was archived and
verified. Build 105 was installed by filesystem copy; build 98 was then restored
to `/Applications/Quarry.app`, verified, and replaced again with the exact build
105. Each version's bundle bytes, signature, and revision matched. Guest `spctl`
accepted `Notarized Developer ID`, and the stapled ticket was present. The guest
rollback archive is `/private/tmp/quarry-beta-105/Quarry-build98-rollback.zip`;
`vm-evidence.json` records the checks. No quarantine attribute was present after
the local transfer. This was file installation and rollback in an existing VM,
not browser download, Finder installation, or offline/fresh-environment launch.
The owner then opened the installed app and fixture. The clean build 105 process
was verified, and a screenshot independently confirmed five rows, six columns,
expected values, and completed indexing. A controlled xattr change while the
file was open changed ctime without altering bytes, mtime, inode, or size.
The owner reported completing VM edit/Undo/Redo/Save As/quit/reopen. Direct checks
confirmed the 274-byte source unchanged and the 279-byte saved output exactly
equal to the original with `Casey` changed to `Casey Beta`, SHA-256
`0b32a5e9652c0db396afaffa76c38002988dd7ed56bcb3e0d56ac3c8a884464c`.
`vm-save-verification.json` records this evidence. Undo/Redo and reopening remain
owner-reported: the final inspection showed Finder with Quarry closed, so the
reopened grid was not independently observed.

VM download and Finder installation are also reported successful. That report
does not identify a build or replace the build 105 guest-channel transfer and
quarantine evidence above.

Fresh-environment launch, broader connected-workflow checks, GUI large-file
checks, supported macOS versions, and browser-download evidence tied to this
build remain pending. Only macOS 26.6.2 ARM64 has native
evidence. Broader checks include more than 64 columns, Unicode/missing fields,
keyboard/accessibility/window size, and selected temporary-folder/insufficient-space
GUI handling. No fresh-VM or browser-quarantine acceptance is claimed. This beta
targets Apple Silicon Macs; Intel, Linux, and Windows are outside its scope.
Release approval remains open. Download details are supplied privately to testers.

This documentation follow-up does not change the frozen `159e149` app, build
number, clean-source status, or archive hashes. The installed local ad-hoc app
and the Developer ID signed candidate remain separately identified artifacts.
