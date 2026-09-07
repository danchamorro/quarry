# Beta release checklist

**Status, 2026-09-07:** beta preparation is in progress. The four pre-beta
features and dialog polish are merged. The clean installed macOS app at
`69d5f1513cb3087710ee4053831f96f8f46948a3` passed the recorded native checks.
This is a validated local build, not a published beta or a supported-platform
commitment. Final owner acceptance and public-distribution gates remain open.

Preparation changes on `codex/beta-preparation` were validated before commit
from a dirty worktree based on `69d5f15`. Their local package passed verification, but it is not a release
candidate. The installed application remains the clean `69d5f15` baseline;
the preparation package has not been installed.

The [pre-beta checklist](PRE_BETA_CHECKLIST.md) holds feature evidence. This
checklist tracks the exact candidate that may be released. Follow the
[packaging guide](MACOS_PACKAGING.md) for build, signing, installation, and
rollback procedures. Keep app bundles local; do not upload them to GitHub.

## Platform matrix

Linux is the priority for platform expansion. Engine or CLI validation does
not establish desktop support, and a successful build does not establish a
minimum supported operating-system version.

| Platform | Current evidence | Beta gate |
|---|---|---|
| macOS, Apple Silicon | Clean installed `69d5f15` validated on macOS 26.6.2, ARM64; local preparation checks and packaging passed | Choose and test the supported macOS versions on the final candidate; complete signing and notarization |
| macOS, Intel | No release acceptance recorded | Decide whether this architecture is in the first beta; if included, build and test it on Intel hardware |
| Linux, core and CLI | Debian 12 ARM64 container and Ubuntu 24.04 x86_64 CI, Rust 1.88.0: 180 tests and locked release builds passed | Validate the final candidate; this does not claim Linux GUI support |
| Linux, desktop | Current check fails because the native-dialog dependency requires a Linux backend | Select and validate the file-dialog backend, then check windowing, keyboard, accessibility, packaging, and native workflows before claiming support |
| Windows | No release acceptance recorded | Plan engine, filesystem, desktop, and packaging validation before claiming support |

The existing plist minimum declaration is not evidence that the application
runs on that macOS version. The candidate's deployment target and actual
supported-system tests must agree. The installed baseline reports Mach-O minimum
OS 11.0 and SDK 26.5; native acceptance has only been recorded on macOS 26.6.2.

## Candidate gates

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
  untested platform explicitly unsupported in its release notes.

### Reproducible build and validation

- [ ] Select the candidate version and exact clean commit. Confirm Cargo and
  bundle version metadata agree, and use that revision for all final evidence.
- [ ] Pass the required locked checks on the candidate:

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
  Finder-open, and drag-and-drop checks.
- [ ] Validate representative large files on the candidate: progressive open,
  navigation, one full-file operation, cancellation, source preservation,
  memory, and temporary-disk use. Link existing 1 GB, 12 GB, and 50 GB reports
  as historical evidence, not measurements of a newer candidate.
- [ ] Verify installation, update, and rollback with the exact candidate.
  Preserve unsaved work before closing the existing app and confirm installed
  revision and clean-source metadata after installation.

### Licenses and trusted distribution

- [ ] Audit the locked dependencies and include Quarry's licenses and required
  third-party notices in the final package. Record the notice artifact and audit
  result without changing the project's licenses or contributor terms. Draft
  inclusion is complete, but attribution review remains open in the
  [notice audit](../packaging/licenses/AUDIT.md). Freshness passes with
  `./scripts/generate-notices.sh --check`; the separate `--release-check`
  deliberately fails until the documented manual review gates are resolved.
- [ ] Obtain a usable Developer ID Application identity and notarization
  credentials. At preparation start no valid signing identity is available;
  the presence of `notarytool` alone does not satisfy this gate.
- [ ] Sign the exact candidate with the required runtime settings, verify it,
  complete notarization, staple the result, and test Gatekeeper acceptance on
  a separate supported Mac. An ad-hoc local signature does not close this gate.
- [ ] Obtain owner approval for the distribution arrangement and public release
  information. This checklist does not choose an offer, price, store, provider,
  or download channel.
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

The current application baseline is PR #40 at `69d5f15`: 296 workspace tests
(115 GUI tests), strict Clippy, formatting, release build, bundle verification,
and local native checks passed. The earlier Filters and remaining-dialog runs
are recorded in the [interface polish evidence](PRE_BETA_CHECKLIST.md#interface-polish-follow-up).
Final owner acceptance is still pending. Signing, notarization, and distribution
approval are not complete.

Preparation validation on the dirty `codex/beta-preparation` worktree passed
formatting, strict workspace Clippy, all 296 workspace tests, and the locked
workspace release build. No Rust application code changed. Packaging self-tests
passed rollback, missing/empty license resources, and five offline notice
generator regressions. Packaging and verification of
`target/package/Quarry.app` passed with a local ad-hoc signature. These checks
establish preparation progress, not final candidate acceptance or installation.

The local package includes Quarry's MIT and Apache licenses and the
[draft third-party inventory](../packaging/licenses/THIRD_PARTY_NOTICES.html)
under `Contents/Resources/Licenses/`. All three resources matched their source
files exactly: `LICENSE-MIT` (1,076 bytes), `LICENSE-APACHE` (11,349 bytes), and
`THIRD_PARTY_NOTICES.html` (158,767 bytes). The inventory covers 142 packages for
the macOS ARM64 dependency selection and includes four font notices.

The [audit](../packaging/licenses/AUDIT.md) records unresolved upstream copyright
and attribution coverage, including AccessKit, the objc2 family, embedded fonts,
and separate Rust/toolchain and platform review. The successful freshness check
does not clear these gaps. The release check currently fails by design, and the
final license gate above remains open.

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

Complete this record for the exact candidate before closing the release gates:

| Evidence | Candidate record |
|---|---|
| Version, full commit, clean-source status | Pending |
| Build toolchain, SDK, deployment target, architecture | Pending |
| Supported OS/hardware acceptance runs | Pending |
| CI and locked validation results | Pending |
| Linux core/CLI evidence, separate from desktop support | Debian 12 ARM64 container and Ubuntu 24.04 x86_64 CI at `5a24874`, Rust 1.88.0: 180 tests and release builds passed; repeat for the final candidate |
| Connected workflow and large-file results | Pending |
| Install/update/rollback and installed revision | Pending |
| Project/dependency notice audit | Draft inventory and three bundled resources verified; freshness passed; release clearance remains unresolved in [AUDIT.md](../packaging/licenses/AUDIT.md) |
| Signed package hash, signing and notarization results | Pending |
| Known issues, feedback triage, and release notes | Pending |
| Final owner acceptance and release authorization | Pending |
