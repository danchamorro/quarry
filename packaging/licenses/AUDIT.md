# Draft macOS notice audit

Status: **UNRESOLVED. This inventory is not release clearance.**

The checked-in HTML covers the `quarry-egui` dependency selection for
`aarch64-apple-darwin`, using the locked workspace manifests and cargo-about 0.9.2.
The initial inventory contains 142 packages, including three Quarry packages.
Build and development dependencies are excluded. This is a dependency inventory,
not proof that every listed dependency contributes code to the final executable.
Quarry's own licenses are packaged separately, without changing their terms.

## Reproduce and check

```sh
./scripts/generate-notices.sh --cargo-about /path/to/cargo-about
./scripts/generate-notices.sh --check
./scripts/generate-notices.sh --release-check
```

Regeneration requires cargo-about **0.9.2**, Cargo, and access to the locked crate
sources. It may fetch dependencies. Obtain cargo-about from its
[official releases](https://github.com/EmbarkStudios/cargo-about/releases/tag/0.9.2)
and verify the release checksum before using it. The audit used the official
`cargo-about-0.9.2-aarch64-apple-darwin.tar.gz` archive with SHA-256
`ae72f0df0c399a1e96336f696fa55b1b28679fd725632eba8cf8e4568467cc3e`.
No global installation is required.

`--check` requires only Python 3 and repository files. It checks content hashes
for the artifact and its inputs, including the lockfile, workspace manifests,
generator, configuration, supplemental notices, and this audit. Success means
freshness and integrity only. It does not review license obligations. Normal local
packaging may include this draft. `--release-check` additionally fails because
the manual review gates below remain unresolved. Neither command publishes files.
Pass `--target <triple>` to assert the package target; targets other than
`aarch64-apple-darwin` fail until a corresponding inventory is implemented.

The generator uses genuine source files selected by cargo-about. It omits
source-less fallback text and the detected MIT placeholder from the HTML, listing
the affected packages explicitly instead. The four original bundled font notices
are preserved in `supplemental.json` and the HTML, including the Hack notice that
contains Source Foundry, DejaVu, and Bitstream terms. Font byte hashes are recorded
and verified against the locked crate on regeneration. Checked-in upstream
supplements cite exact repository commits and retain the provided text unchanged.
Regeneration verifies their recorded hashes; changes to those supplements require
a new source audit. HTML escaping changes markup representation, not rendered text.

## Open release gates

- Resolve every cargo-about fallback and placeholder listed in the generated
  HTML. Some have a supplemental upstream notice, but a supplemental file alone
  does not establish that all notices and copyright statements are present.
  This includes shared-repository crates whose crate archives omit root licenses.
- Review source-level copyright and attribution. AccessKit source headers refer
  to Chromium's `LICENSE.chromium` and derived kurbo code. The crate archive does
  not contain the referenced Chromium file. Its upstream MIT file has no
  copyright header. That is a concrete gap, not permission to invent a holder.
- Review the objc2 family's actual permission and copyright notices. The captured
  newer upstream `LICENSE.md` explains license choices and flags Apple SDK derived
  code questions; it is not itself a replacement for complete permission texts.
  Older objc2 packages and `dispatch` also remain in the fallback list.
- Check embedded font attribution beyond crate SPDX expressions. All four
  distributed notice files are included, but font metadata and upstream required
  copyright notices still need comparison. The Hack notice includes Bitstream
  terms absent from the crate's top-level expression.
- Review Rust standard-library, compiler runtime, build-generated code, native
  libraries, and Apple SDK/platform requirements against the actual release
  binary and toolchain. Cargo-about does not cover these automatically. Toolchain
  and application source changes do not invalidate this dependency-only inventory,
  so this separate review is required for each release candidate.
- Re-run and review the appropriate target inventory before packaging Intel
  macOS, Linux, or Windows. This artifact covers only Apple Silicon macOS.

These are engineering coverage findings, not legal conclusions. A successful
automatic license scan cannot close them. Record the reviewed release target,
toolchain, binary, notice coverage, and remaining obligations before replacing the
draft status and changing the release check.
