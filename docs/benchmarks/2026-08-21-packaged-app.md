# Phase 7A packaged-app validation: 2026-08-21

## Status

The repeatable package workflow, canonical installation, rollback archive,
strict signature verification, installed-app interaction journey, and clean
release gate pass. Commit `8878708da7efb6daba62714c839c012dd269f545`
was packaged and installed with `QuarrySourceStatus=clean` and
`QuarryGitRevision` equal to commit
`8878708da7efb6daba62714c839c012dd269f545`. This evidence update is
documentation-only and follows the validated implementation commit.

## Environment

| Item | Value |
|---|---|
| Machine | MacBook Pro, Apple M3 Max, 16 cores |
| Memory | 128 GB |
| Operating system | macOS 26.6.1 (25G76) |
| Architecture | arm64 |
| Rust | 1.88.0 |
| Xcode | 26.6 (17F113) |
| macOS SDK | 26.5 |
| App version | 0.1.0 |
| Build version | 31 |
| Validation source revision | `8878708da7efb6daba62714c839c012dd269f545` |
| Installed path | `/Applications/Quarry.app` |
| Bundle identifier | `io.github.danchamorro.quarry` |
| Signature | Ad hoc, strict verification passed |

## Package and installation results

Two consecutive package commands from the same checkout, pinned Rust 1.88.0
toolchain, macOS SDK, and machine produced identical payload hashes:

| Payload | SHA-256 |
|---|---|
| `Contents/Info.plist` | `a6242ce9a03eb4bf78cf5d347b7c595ec856611fea6eed0861b434abd0990849` |
| `Contents/MacOS/Quarry` | `e55c5a24fa8dc213160d0265ff65b9bf3ac1018f840e2af28f664d7ab4a90927` |
| `Contents/Resources/Quarry.icns` | `b252894d5a3a2c7aa76bf1dc76b5ffe483a55406d7d4032498c190021048aecb` |

The installer verified the candidate before replacement, installed an exact
plist and executable match, passed `codesign --verify --deep --strict`, removed
the old `/Applications/Quarry Egui.app`, and removed its staged candidate. The
preceding canonical app's rollback archive was extracted before publication and
its plist, executable, and strict signature were verified. The retained legacy
prototype archive was also extracted and passed the same checks.

A separate running-app check refused the update immediately with `Quit Quarry
before installing an update.` and created no staged app. The same guard rejected
a process named `quarry-egui`, which is the Cargo development executable.
Build 31 also held its shared application installation lock for the full live
process lifetime: a competing exclusive lock returned status 75. With that
exclusive lock held first, the build exited before opening a window or input
file. Process-name checks remain as migration coverage for older builds that do
not participate in the lock.

The rollback self-test replaced a disposable installed marker, invoked the same
rollback function used by the exit and signal traps, and restored only the
previous marker. CI runs this check before packaging.

A held per-user package lock caused a concurrent package command to exit with
status 75 before building or changing the shared candidate.

## Installed-app interaction journey

The test used this 55-byte source:

```csv
key,name
b,first-b
A,upper
,missing
a,lower
b,second-b
```

From `/Applications/Quarry.app`:

1. The source opened and indexed five data rows.
2. `first-b` was edited in place to `first-b edited`.
3. Column 1 was sorted ascending with case-sensitive stable text semantics.
4. The grid immediately showed `missing`, `upper`, `lower`, `first-b edited`,
   and `second-b` in that order.
5. Save As published `/private/tmp/quarry-phase7-smoke-output.csv`.
6. The source still matched its original bytes exactly.
7. The 62-byte output matched the expected bytes exactly.
8. Quarry quit, relaunched from the canonical application, reopened the output,
   and showed the same clean values and order.

After the review fix, clean build 31 launched from
`/Applications/Quarry.app`, held the application installation lock, and quit
normally. The complete interaction journey above remains the UI evidence; the
review fix changes only startup installation exclusion, locked CI resolution,
and toolchain selection.

| File | SHA-256 |
|---|---|
| Unchanged source | `e236f1a14b2761eb593617bd8f80f5834a5ae897d38dfe74bb48b13b51d7886d` |
| Saved output | `903d15aad5251b9ade79aad1552bf1a27d4ba0eb37ae49a8f912f2fae3a20be4` |

## Gate

- [x] Repeated package payload hashes match in the recorded environment.
- [x] Candidate and installed bundle verification pass.
- [x] A running app blocks update before packaging or replacement begins.
- [x] The installed app and installer mutually exclude startup and bundle
  replacement through one persistent application lock.
- [x] The installer rollback self-test restores the prior disposable app state.
- [x] A concurrent package command is rejected by the shared operation lock.
- [x] Only `/Applications/Quarry.app` remains installed.
- [x] A verified rollback archive exists outside `/Applications`.
- [x] Open, edit, sort, Save As, exact source preservation, exact output, quit,
  relaunch, and reopen pass from the installed app.
- [x] Rebuild and install from the committed Phase 7A revision with a clean
  source marker.

Ad-hoc signing is sufficient only for this local alpha validation. This record
does not claim Developer ID signing, notarization, or readiness for distribution
to other Macs.
