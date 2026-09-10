# Beta release checklist

**Status, 2026-09-10:** the metadata-warning fix merged in
[PR #43](https://github.com/danchamorro/quarry/pull/43) as
`159e14950f6b4694129a810e19299c2f7fb950df`. The new frozen clean candidate is
**0.1.0 (105)**. Developer ID signing, notarization, stapling, local Gatekeeper,
and extracted-bundle verification passed. The same clean revision is installed
locally with a verified rollback backup. Its native picker, controlled
metadata-only change, Undo/Redo, exact Save As, quit, and reopen checks passed.
A fresh 1 GB CLI/core filter/export/cancellation run also passed.
The signed app extracted from the final ZIP passed native opening, cell/header
Undo/Redo, exact Save As, combined filtering/export, sorting, duplicate removal,
and quit/reopen with source preservation.
The existing VM also passed signed-package file installation, rollback to
build 98, and restoration of build 105 with exact-byte and signature checks.
Its installed build 105 launched and rendered the five-row/six-column fixture.
The owner reported completing the VM edit/Undo/Redo/Save As/quit/reopen workflow;
saved output and source preservation were independently verified.

The invited beta targets Apple Silicon Macs; only macOS 26.6.2 ARM64 has
acceptance evidence. Other macOS versions remain untested. Intel, Linux, and
Windows are outside this beta's scope. Broader connected-workflow checks,
fresh-environment acceptance, supported macOS versions, final owner acceptance,
and browser-download evidence tied to this build remain open. The earlier
`cbd9446` candidate is historical; subsequent documentation commits do not
change either frozen package.

Historical preparation changes on `codex/beta-preparation` were validated before
commit from a dirty worktree based on `69d5f15`. Their local package passed
verification, but it is not a release candidate. After PR #41 merged, the clean
`b461771` build was installed and
verified; its rollback archive preserved the prior clean `69d5f15` app.
The notice-audit follow-up merged in [PR #42](https://github.com/danchamorro/quarry/pull/42).

The [pre-beta checklist](PRE_BETA_CHECKLIST.md) holds feature evidence. This
checklist tracks the exact candidate that may be released. The
[tester guide](BETA_TESTER_GUIDE.md) describes installation, test workflows,
and feedback. Download details are supplied privately to invited testers.
Follow the [packaging guide](MACOS_PACKAGING.md) for build, signing, installation, and
rollback procedures. Do not upload app bundles to GitHub.

## Platform matrix

Linux is the priority for platform expansion. Engine or CLI validation does
not establish desktop support, and a successful build does not establish a
minimum supported operating-system version.

| Platform | Current evidence | Beta gate |
|---|---|---|
| macOS, Apple Silicon | Clean `159e149`, version 0.1.0 (105), signed and notarized; installed app and signed ZIP passed bounded native workflows on macOS 26.6.2 ARM64 | Choose supported macOS versions and complete remaining exact-candidate acceptance |
| macOS, Intel | No release acceptance recorded | Outside the invited Apple Silicon scope; requires its own build and hardware acceptance before support |
| Linux, core and CLI | Source tree matching `159e149`: Debian 12 ARM64, Rust 1.88.0, 184 tests and locked release build passed; older Ubuntu x86_64 CI evidence remains below | Outside the invited Mac scope; this does not claim Linux GUI support |
| Linux, desktop | Current check fails because the native-dialog dependency requires a Linux backend | Select and validate the file-dialog backend, then check windowing, keyboard, accessibility, packaging, and native workflows before claiming support |
| Windows | No release acceptance recorded | Plan engine, filesystem, desktop, and packaging validation before claiming support |

The existing plist minimum declaration is not evidence that the application
runs on that macOS version. The candidate's deployment target and actual
supported-system tests must agree. The installed baseline reports Mach-O minimum
OS 11.0 and SDK 26.5; native acceptance has only been recorded on macOS 26.6.2.

## Candidate gates

### Source metadata follow-up

PR #43, merged as `159e149`, fixes metadata-only false alarms in
the shared source guard used by edit Undo/Redo, Save, and private rewrites. On
macOS, equal valid file-data generation counts permit ctime-only changes. Real
rewrites, path replacement, size, modification time, permissions, and ownership
changes remain guarded; unavailable counters keep conservative ctime checks.

Validation before commit passed 303 macOS workspace tests, strict Clippy,
formatting, locked release builds, packaging self-tests, and bundle verification.
A Debian 12 ARM64 container passed 184 core/CLI/parser tests and locked release
builds. Regressions cover metadata changes before startup and publication,
same-length external writes with restored mtime, and unsupported/invalid counters.
The regenerated notice HTML and all dependency versions are unchanged.

The earlier local ad-hoc package (dirty source based on `98c5747`, not an
installed release candidate) passed a controlled native check: edit, add an xattr without changing
CSV bytes or mtime, Undo, Redo, exact Save As, quit, and reopen. The original
CSV was preserved. This test used LaunchServices file opening after a computer-use
capture error in the native picker; it does not close file-picker acceptance.
The new clean candidate now contains this fix and passed signing/notarization.
The installed clean build also passed the native picker and the controlled
metadata-change workflow. The remaining acceptance gates below still apply.

### Product acceptance

- [x] Complete the four pre-beta feature priorities and dialog polish. The
  latter merged in [PR #40](https://github.com/danchamorro/quarry/pull/40) as
  `69d5f15`; installation and native verification passed from that clean commit.
- [ ] Complete the owner's final acceptance of the connected workflow below.
  Approval to merge or begin release preparation does not close this gate.
- [ ] Triage candidate defects. Resolve data loss, unintended source changes,
  corrupted output, and failed cancellation or recovery before release. Record
  accepted nonblocking issues with their workarounds in candidate release notes.
- [ ] Confirm the platform and architecture scope for this beta. Leave every
  untested platform explicitly unsupported in its release notes. The owner
  selected Apple Silicon Macs for the invited group; only macOS 26.6.2 has
  acceptance evidence so far, and supported macOS versions remain to be confirmed.

### Reproducible build and validation

- [x] Select the candidate version and exact clean commit. Cargo and bundle
  version are 0.1.0, bundle build is 105, and the candidate records clean
  `159e14950f6b4694129a810e19299c2f7fb950df`. Subsequent documentation commits
  do not change this frozen candidate or its recorded revision.
- [x] Pass the required locked source checks. The merge tree exactly matches
  validated PR head `c37b797`: formatting, strict Clippy, 303 workspace tests,
  locked release builds, and packaging self-tests passed before merge. New
  clean-candidate package creation and bundle verification also passed. Source
  checks and exact-package acceptance are separate evidence:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
  cargo test --workspace --locked
  cargo build --workspace --release --locked
  ./scripts/macos-app.sh self-test
  ./scripts/macos-app.sh package
  ./scripts/macos-app.sh verify target/package/Quarry.app
  ```

- [ ] Run the connected workflow on every supported macOS version and
  architecture. Record keyboard, accessibility, smaller-window, relaunch,
  Finder-open, and drag-and-drop checks. The signed build passed the bounded
  six-column workflow below; wide-column, Unicode/missing-field, full keyboard,
  accessibility/window-size, and temporary-storage GUI checks remain open.
- [ ] Validate representative large files on the candidate: progressive open,
  navigation, one full-file operation, cancellation, source preservation,
  memory, and temporary-disk use. Link existing 1 GB, 12 GB, and 50 GB reports
  as historical evidence, not measurements of a newer candidate. A new 1 GB
  CLI/core run at `159e149` passed; GUI large-file acceptance remains open.
- [ ] Verify installation, update, and rollback with the exact candidate.
  Preserve unsaved work before closing the existing app and confirm installed
  revision and clean-source metadata after installation. The local ad-hoc
  `159e149` installation and rollback-backup verification passed. The existing
  VM passed filesystem installation of the signed ZIP, restoration of build 98,
  and reinstatement of build 105, with exact bytes and signatures verified.
  VM launch/rendering passed. The owner reported completing edit/Undo/Redo/
  Save As/quit/reopen, with exact saved output and unchanged source independently
  verified. VM download/Finder installation is also reported successful.
  That report does not identify a build or record browser-quarantine metadata.

### Licenses and trusted distribution

- [x] Audit the locked dependencies and include Quarry's licenses and required
  third-party notices in the final package. Record the notice artifact and audit
  result without changing the project's licenses or contributor terms. The
  locked ARM64/Rust 1.88.0 source inventory is reviewed in the
  [notice audit](../packaging/licenses/AUDIT.md), including the five resolved
  notice gaps. Freshness uses `./scripts/generate-notices.sh --check`;
  `--release-check` also requires the recorded review hash to match the exact
  manifest. The clean candidate passed that gate; its ten license resources
  matched the reviewed files, all four embedded fonts were verified, and native
  links were confined to Apple system libraries and frameworks.
- [x] Obtain a usable Developer ID Application identity and notarization
  credentials. Completed on 2026-09-07: signing succeeded and the validated
  Keychain profile authenticated an accepted local trial submission.
- [x] Sign the exact candidate with the required runtime settings, verify it,
  complete notarization, and staple the result. Apple accepted clean `159e149`
  with no issues; signature, staple, and local Gatekeeper checks passed.
- [ ] Test that exact candidate's Gatekeeper acceptance on a fresh supported
  macOS environment (another Mac or a clean VM). The earlier dirty trial's
  successful offline VM launch does not close this gate.
- [ ] Obtain owner approval for the distribution arrangement and public release
  information. Private download links and tester records are maintained outside
  this repository.
- [ ] Approve candidate release notes containing the supported-system matrix,
  known limits, update/rollback guidance, and feedback link. Publish only after
  the preceding release gates and explicit owner authorization are complete.

## Connected acceptance workflow

Use a disposable copy of a synthetic or shareable file. Record the source hash
before starting and compare it after every operation that should preserve it.
Include quoted delimiters, embedded newlines, Unicode, blank and missing fields,
duplicate values, and numeric boundaries. Repeat navigation and column controls
with more than 64 columns.

1. Open through Finder and the app, confirm detected format, then check
   progressive rows, navigation, source-column numbers, and cancellation.
2. Edit a cell and header; use Undo/Redo; preserve a multiline value. Save As a
   new path and verify exact expected output while the original remains intact.
3. Search, hide, reorder, reset, and auto-fit Columns. Reopen the window and
   confirm identities and view settings. Check keyboard focus and Escape.
4. Use a text rule and inclusive numeric Between. Check picker search, invalid
   bounds, active-filter preservation, Clear, filtered export, and exact rows.
5. Exercise Sort options and Details. Verify output order, retained rows,
   header preservation, cancellation, and Undo/Redo.
6. Find duplicates by selected columns. Review counts, cancel once, then find
   again and explicitly remove extras. Verify keep-first behavior and Undo/Redo.
7. Exercise a selected temporary folder and insufficient-space handling.
   Cancellation or failure must leave the document usable and source intact.
8. Save a disposable working copy, compare exact output, quit, relaunch, and
   reopen it. Check that unpublished temporary files are cleaned up and that
   required Undo files are retained while the document still needs them.

For each run record pass/fail, candidate revision, OS/architecture, fixture
shape and size, expected/actual output, source preservation, and any issue link.
Do not treat visual row counts alone as proof of exact export or save output.

## Tester feedback and current limits

Use the [bug report form](https://github.com/danchamorro/quarry/issues/new?template=bug_report.yml)
for failures or unexpected behavior. Include the build revision, operating
system and architecture, steps, expected and actual behavior, and whether the
source or unsaved work was affected. For slow operations, include file size,
row/column counts when known, elapsed time, RAM, available disk, and the visible
operation phase.

GitHub issues are public. Use synthetic examples and redact file paths,
screenshots, logs, and values. Do not upload production CSVs, personal data,
credentials, or confidential files. A small reproduction is helpful when it
can be shared safely; an original dataset is not required.

Current boundaries to include in candidate notes:

- One document per application window; CSV append, document tabs, and a terminal
  interface are future work. See the [ordered follow-ups](ROADMAP.md#post-beta-sequence).
- Editing and deleting rows require clearing an active filter. Unsaved cell
  edits must be saved or discarded before filtering.
- Structural history retains the adjacent working version. Individual edit
  history has the documented count and byte limits in
  [Undo and Redo](USER_GUIDE.md#undo-and-redo-changes).
- Date/time and multi-column sorting are not implemented. Numeric sorting
  requires valid supported numbers; numeric filters skip invalid data values.
- Case-insensitive matching folds ASCII letters. Non-UTF-8 and missing-cell
  editing and other format limits remain as described in the
  [user guide](USER_GUIDE.md).
- Storage checks are advisory. Save and export still need staging space on
  the destination volume, even with another temporary working folder selected.

## Release evidence

### Current frozen candidate, 2026-09-08

Version 0.1.0 (105) is frozen at clean
`159e14950f6b4694129a810e19299c2f7fb950df`, tree
`bace7d5898807191030309959e33852cc1c28750`. It contains the PR #43 source guard
fix. That tree exactly matches validated PR head `c37b797`, with 303 macOS
workspace tests and 184 Linux core/CLI/parser tests; these prior source checks
are distinct from the new package checks. The package uses Rust 1.88.0,
LLVM 20.1.5, SDK 26.5, and ARM64. Its ten license resources matched the reviewed
files; the third-party notice HTML is unchanged.

The installed ad-hoc app reports the same clean revision and build. The
installer preserved and verified `Quarry-previous.zip`. Native picker opening,
a controlled ctime-only xattr change, Undo/Redo without reload, exact Save As,
quit, and native-picker reopen passed. The original CSV remained intact; the
279-byte saved file has SHA-256
`0b32a5e9652c0db396afaffa76c38002988dd7ed56bcb3e0d56ac3c8a884464c`.
This installed-app result is separate from signed-candidate GUI acceptance.

The signed app extracted from the final ZIP then passed native picker opening,
cell and header Undo/Redo, and the controlled xattr change without reload.
Exact Save As preserved both edits and multiline data. Combined Amount Between
100 and 1000 inclusive and Status Equals Active produced three matches; both
column pickers and picker search worked. The exact filtered export was 189 bytes,
SHA-256 `32ce54955bab5c76fe29c1ffca3ac07098d702384e9b6fcab0891c4ed8e8d7f5`.
Numeric ascending sort and duplicate removal passed Undo/Redo and exact saved
output checks. Duplicate review Cancel preserved five rows; removing the extra
kept the first occurrence and left four. Quit was verified by process absence,
then native-picker reopen showed a clean four-row/six-column file retaining
the edited header, cell, and multiline value. Source bytes remained unchanged.
This is bounded local host acceptance, not a fresh-environment or VM run.

The existing Parallels macOS 26.6.2 ARM64 VM received the exact ZIP through the
local guest channel, with matching SHA-256. Its clean build 98 backup was
verified, build 105 was installed by filesystem copy, build 98 was restored to
Applications, then build 105 was reinstated. Each installed version's revision,
signature, and bundle bytes were verified. Guest Gatekeeper assessment accepted
`Notarized Developer ID`, and the stapled ticket was present. The transferred
app had no quarantine attribute. This establishes file installation and rollback,
not browser-download, Finder installation, offline first-launch, or fresh-VM
acceptance. After the owner opened the app, its installed clean build 105 process
was verified and a screenshot independently confirmed five rows, six columns,
expected values, and completed indexing. A controlled xattr change while the
file was open changed only ctime; bytes, mtime, inode, and size were unchanged.
The owner reported completing the VM edit/Undo/Redo/Save As/quit/reopen workflow.
Direct verification confirmed the 274-byte original unchanged and the 279-byte
`beta-check-edited.csv` exactly equal to the original with `Casey` changed to
`Casey Beta`, SHA-256
`0b32a5e9652c0db396afaffa76c38002988dd7ed56bcb3e0d56ac3c8a884464c`.
Undo/Redo and reopening are owner-reported; the final inspection showed Finder
with Quarry closed, so the reopened grid was not independently observed.

The newly rebuilt CLI passed nine operations on the existing 1,000,000,155-byte,
5,179,437-record synthetic fixture. Independent Decimal counts and every raw
exported record matched: 1,724,748 records, 334,036,832 bytes, SHA-256
`15ae1c8da605746863e3e3982c4ab6266355e64d4dd8c81c0cc21ff63595ccc9`.
The source hash and all 20 recorded source inputs were unchanged. Filter and
export cancellation stopped after 101,711,872 bytes without publishing a
destination or leaving export temporary files. Worker times were 0.955 s and
1.194 s, with CLI-reported peak RSS of 26.77 MiB and 4.30 MiB. This warm-cache
run validates CLI/core behavior, not GUI or larger-than-RAM performance.

Apple accepted submission `30a565de-89ff-44ed-8ad9-dd74309250d1` without issues.
Signature, staple, local Gatekeeper, extracted-file comparison, extracted-ticket,
and bundle verification passed. The final stapled ZIP is 3,611,149 bytes,
SHA-256 `31e72cb0b8cafb62bca76e7d40b00ffcb6638fc10d5ca3100205e2ce8e7d80fb`.
Local evidence is under `target/quarry-beta-candidate.159e149.q4a2Kw/`:
`candidate-evidence.json`, `build-evidence.json`, `installed-evidence.json`,
`installed-ui-evidence.json`, `candidate-ui-evidence.json`, `notarization-log.json`,
`vm-evidence.json`, `vm-save-verification.json`, and
`large-file/acceptance-summary.json`. Fresh-environment and broader workflow
acceptance remain pending. No invitations or package publication are authorized
by these checks.

### Historical preparation and candidate records

The records below describe earlier builds and their state at the time. They do
not replace the current `159e149` record or establish its outstanding acceptance.

The application feature baseline is PR #40 at `69d5f15`: 296 workspace tests
(115 GUI tests), strict Clippy, formatting, release build, bundle verification,
and local native checks passed. The earlier Filters and remaining-dialog runs
are recorded in the [interface polish evidence](PRE_BETA_CHECKLIST.md#interface-polish-follow-up).
Final owner acceptance and distribution approval are still pending. The clean
candidate's signing and notarization results are recorded separately below.

Preparation validation on the dirty `codex/beta-preparation` worktree passed
formatting, strict workspace Clippy, all 296 workspace tests, and the locked
workspace release build. No Rust application code changed. Packaging self-tests
passed rollback, missing/empty license resources, and five offline notice
generator regressions. Packaging and verification of
`target/package/Quarry.app` passed with a local ad-hoc signature. These checks
establish preparation progress, not final candidate acceptance or installation.

The initial preparation package included Quarry's MIT and Apache licenses and the
[draft third-party inventory](../packaging/licenses/THIRD_PARTY_NOTICES.html)
under `Contents/Resources/Licenses/`. Its three resources matched their source
files exactly: `LICENSE-MIT` (1,076 bytes), `LICENSE-APACHE` (11,349 bytes), and
`THIRD_PARTY_NOTICES.html` (158,767 bytes). The inventory covers 142 packages for
the macOS ARM64 dependency selection and includes four font notices.

The [audit](../packaging/licenses/AUDIT.md) records source-backed additions
for AccessKit, older/inherited objc2 notices, other crate attribution, embedded
font metadata, and the actual Rust runtime. The five previously open gaps are
resolved by preserving dispatch 0.2.0's explicit MIT declaration and canonical
permission terms, and selecting the offered Zlib alternative for dispatch2
0.3.1 and objc2 AppKit/CoreFoundation/CoreGraphics 0.3.2. The inventory contains
55 supplemental records. Its separate review hash becomes stale when regenerated
inputs change. The expanded package includes seven Rust resource files and rejects a
compiler release without matching runtime notices.

Before the final five-gap resolution, the `codex/beta-license-audit` follow-up
passed formatting, strict Clippy, all
296 workspace tests, the locked release build, and nine notice regressions.
Package creation and signature verification passed from the dirty worktree.
All ten bundled license resources matched their source files exactly: the
expanded third-party HTML is 219,567 bytes, and seven Rust files total 462,709
bytes. Missing/empty resource checks cover the full list. A packaging probe
from another directory confirmed that `RUSTC` is queried inside the build
checkout and an unsupported compiler is rejected before replacing the package.
The installed app remains the clean `b461771` baseline during this follow-up;
these package checks are not final candidate acceptance.

After resolving the five recorded gaps, formatting, strict workspace Clippy,
all 296 workspace tests, the locked release build, and packaging self-tests
passed again. The notice suite now has 12 tests, including missing/stale review
records and rejection of a license alternative the package does not offer.
All 55 supplemental texts were verified in the generated HTML. The inventory
review is recorded separately from the clean candidate's package and signing
evidence. Candidate-specific results belong with the local candidate artifacts;
the source review does not substitute for final owner acceptance.

The [local notarization trial](MACOS_PACKAGING.md#local-notarization-trial-2026-09-07)
on 2026-09-07 passed Developer ID signing, hardened runtime, secure timestamping,
native CSV open/index/render, Apple notarization, ticket stapling/validation,
and local Gatekeeper assessment. Apple reported no issues. The trial used the
dirty `b461771` package and remained local; the installed host app was not
replaced. The owner also confirmed successful first launch in a fresh
Parallels macOS VM with its network disconnected before opening Quarry.
Read-only guest checks confirmed macOS 26.6.2 (25G83), ARM64, and the expected
dirty trial revision. The owner reported all five requested functional checks
passed: open, combined text/numeric filtering, duplicate removal and undo,
numeric sorting, and edit/Save As/quit/reopen. These are owner-reported workflow
results, not a completed computer-use run. Repeat the release gates for the
final clean candidate, including acceptance in a fresh supported macOS
environment (another Mac or a clean VM).

The clean local candidate at `cbd9446fb34b3b08d46d9fa85a07d77d4a5256d5`,
version 0.1.0 (98), passed the locked validation and reviewed notice gate.
Its signed app opened a synthetic CSV through the native picker, indexed five
records and six columns, rendered multiline values, and quit without edits;
the fixture hash was unchanged. Apple accepted notarization submission
`a2bd2114-03b8-4221-98bd-a785c2e9c2ae` with no issues. Signature verification,
staple validation, local Gatekeeper assessment (`Notarized Developer ID`),
and the comparison after ZIP extraction passed. The final stapled ZIP is
3,608,251 bytes, SHA-256
`13d0d467d76e394faa4243d1602d8a0d9d0afea40b78f6da1b8d2d02e75355f2`.

The local directory `target/quarry-beta-candidate.cbd9446/` contains
`candidate-evidence.json`, `build-evidence.json`, and `notarization-log.json`.
The [packaging record](MACOS_PACKAGING.md#clean-local-candidate-2026-09-07)
distinguishes submitted and stapled archives. These frozen artifacts remain
at `cbd9446` when this evidence is documented in a subsequent source commit.
They have not replaced `/Applications/Quarry.app` or been published. The clean
candidate still needs fresh-environment, full connected-workflow, large-file,
installation/update/rollback, supported-system, and owner acceptance checks.

After downloading the final clean candidate for the requested VM retest, the
owner reported: "Tested on VM and working." This records a successful
owner-reported retest in the existing macOS 26.6.2 ARM64 VM. It does not claim
a newly created test environment or independently verified per-operation output.
The macOS and Linux CI jobs, CLA, and secret scanning for
[PR #42](https://github.com/danchamorro/quarry/pull/42) at `5d7fc88` passed.
CodeRabbit CLI completed with zero findings; its separate GitHub review raised
one packaging-preflight issue. The preflight now checks the configured
`${RUSTC:-rustc}` executable, matching the compiler used by the build path.
The full verification command passed with an absolute compiler path containing
spaces and no `rustc` on `PATH`; a missing configured compiler was rejected.

A bounded 1 GB CLI/core acceptance run reused the synthetic numeric-filter
fixture (1,000,000,155 bytes, 5,179,437 records). The CLI was rebuilt at
`919550b`; CLI/core/delimited source, manifests, lockfile, and toolchain inputs
matched `cbd9446` before and after the run. All five numeric filter counts matched
the independent Decimal oracle. The combined Between/TX export contained
1,724,748 records and 334,036,832 bytes; every raw record matched in source order,
with SHA-256 `15ae1c8da605746863e3e3982c4ab6266355e64d4dd8c81c0cc21ff63595ccc9`.
The source SHA-256 stayed
`b93bdd33d096f195105ced2eb07155d4ac880d7bcb15e2c221d30ace085c2126`.
Filter and export cancellation both stopped after 101,711,872 bytes; neither a
cancelled destination nor export temporary artifacts remained. Worker times
were 1.014 s for filtering and 1.306 s for export; CLI-reported peak RSS was
28.80 MiB and 4.27 MiB respectively. This warm-cache run validates the shared
engine, not GUI navigation or larger-than-RAM performance. It used the existing
[benchmark verifier](benchmarks/2026-09-05-numeric-filters.md), retaining its
published constants and byte/cancellation assertions. Detailed local evidence
is in `/private/tmp/quarry-beta-numeric-253tegqy/acceptance-summary.json`.

Linux core/CLI validation uses the following bounded package scope, without the
desktop crates. A Debian 12 Docker container on `aarch64-unknown-linux-gnu` with
Rust 1.88.0 passed all 180 tests (core 142, CLI 29, delimited 9) and the locked
release build. The same commands are configured in
[Linux CI](../.github/workflows/ci.yml). At `5a24874`, the
[PR validation run](https://github.com/danchamorro/quarry/actions/runs/34153433796)
also passed all 180 tests and the release build on Ubuntu 24.04 x86_64; its
macOS 26 ARM64 job passed workspace checks and packaging. A separate Linux desktop check fails in `rfd` 0.17.2
because neither the `gtk3` nor `xdg-portal` backend is selected. Fixing that build
gate will still require native GUI validation.

```bash
cargo test -p quarry-core -p quarry-delimited -p quarry-cli --locked
cargo build -p quarry-core -p quarry-delimited -p quarry-cli --release --locked
```

### Current candidate record

Complete this record for the exact candidate before closing the release gates:

| Evidence | Candidate record |
|---|---|
| Version, full commit, clean-source status | 0.1.0 (105), `159e14950f6b4694129a810e19299c2f7fb950df`, clean |
| Build toolchain, SDK, deployment target, architecture | Rust 1.88.0, LLVM 20.1.5, SDK 26.5, Mach-O minimum macOS 11.0, ARM64; minimum declaration is not supported-OS acceptance |
| Supported OS/hardware acceptance runs | Host native workflows and existing-VM installation/rollback/launch passed on macOS 26.6.2 ARM64 only; VM edit/Undo/Redo/save/reopen owner-reported with exact output/source verification; broader workflow, supported-system scope, and fresh-environment acceptance remain open |
| CI and locked validation results | Merge tree equals validated PR head `c37b797`: formatting, strict Clippy, 303 workspace tests, locked releases, and packaging self-tests passed before merge; new package and extracted-bundle verification passed |
| Linux core/CLI evidence, separate from desktop support | Matching source tree: Debian 12 ARM64, Rust 1.88.0, 184 tests and locked release build passed |
| Connected workflow and large-file results | Signed ZIP passed native picker, metadata-only change, cell/header Undo/Redo, exact Save As, combined filter/export, sort, duplicates, and quit/reopen; new 1 GB CLI/core filtering/export/cancellation passed at `159e149`; broader GUI workflow and GUI large-file acceptance remain open |
| Install/update/rollback and installed revision | Host ad-hoc clean `159e149`, build 105, installed with verified backup; existing VM signed-file installation, actual rollback to 98, restoration of 105, and launch/rendering verified; VM requested workflow owner-reported with verified saved/source bytes; download/Finder installation also reported successful without a build identifier |
| Project/dependency notice audit | Locked ARM64/Rust 1.88.0 source review recorded in [AUDIT.md](../packaging/licenses/AUDIT.md), 55 supplemental records and ten verified candidate resources; four embedded fonts and system-only native links verified |
| Signed package hash, signing and notarization results | Stapled ZIP: 3,611,149 bytes, SHA-256 `31e72cb0b8cafb62bca76e7d40b00ffcb6638fc10d5ca3100205e2ce8e7d80fb`; submission `30a565de-89ff-44ed-8ad9-dd74309250d1` accepted without issues; signature, staple, local Gatekeeper, extracted bytes/ticket, and bundle checks passed; fresh-environment acceptance pending |
| Known issues, feedback triage, and release notes | [Invited tester guide](BETA_TESTER_GUIDE.md) prepared; guide approval pending |
| Final owner acceptance and release authorization | Pending |
