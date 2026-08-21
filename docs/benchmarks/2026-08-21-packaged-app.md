# Phase 7A packaged-app validation: 2026-08-21

## Status

The repeatable package workflow, canonical installation, rollback archive,
strict signature verification, installed-app interaction journey, and clean
release gate pass. Commit `493b20d521e116be5eb327f8a38bc042608daf11`
was packaged and installed with `QuarrySourceStatus=clean` and
`QuarryGitRevision` equal to commit
`493b20d521e116be5eb327f8a38bc042608daf11`. This evidence update is
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
| Build version | 29 |
| Validation source revision | `493b20d521e116be5eb327f8a38bc042608daf11` |
| Installed path | `/Applications/Quarry.app` |
| Bundle identifier | `io.github.danchamorro.quarry` |
| Signature | Ad hoc, strict verification passed |

## Package and installation results

Two consecutive package commands from the same checkout, Rust toolchain, macOS
SDK, and machine produced identical payload hashes:

| Payload | SHA-256 |
|---|---|
| `Contents/Info.plist` | `98946407b147e802a824d62581cbd1941ac7b2c653b1aae761e87ca88659e31d` |
| `Contents/MacOS/Quarry` | `f5c1f259beb3e3f2b539201371748691c5d87feb91dbfda5420b4cdf117a96e1` |
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

After the clean release install, build 29 was launched from
`/Applications/Quarry.app` and reopened the saved output with the same five
values in the recorded order.

| File | SHA-256 |
|---|---|
| Unchanged source | `e236f1a14b2761eb593617bd8f80f5834a5ae897d38dfe74bb48b13b51d7886d` |
| Saved output | `903d15aad5251b9ade79aad1552bf1a27d4ba0eb37ae49a8f912f2fae3a20be4` |

## Gate

- [x] Repeated package payload hashes match in the recorded environment.
- [x] Candidate and installed bundle verification pass.
- [x] A running app blocks update before packaging or replacement begins.
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
