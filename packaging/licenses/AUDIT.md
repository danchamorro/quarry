# macOS notice audit

Status, 2026-09-07: **Source collection reviewed for the locked macOS ARM64
inventory and Rust 1.88.0. The five previously recorded gaps are resolved below.**
This bounded engineering audit does not change licenses or authorize distribution.

The checked-in HTML covers the `quarry-egui` dependency selection for
`aarch64-apple-darwin`, using the locked workspace manifests and cargo-about 0.9.2.
The initial inventory contains 142 packages, including three Quarry packages.
Build and development dependencies are excluded. This is a dependency inventory,
not proof that every listed dependency contributes code to the final executable.
Quarry's own licenses are packaged separately, without changing their terms.

The inspected executable was clean commit
`b461771e6fe8b78be398208a9830d8c3e2c041c7`, SHA-256
`99461f9077b44948c86b551664f77f38ff9fae8fd5eb0815e35eea33b9ddf19c`, built with
Rust 1.88.0 (`6b00bc3880198600130e1cf62b8f8a93494488cc`), LLVM 20.1.5.
These are recorded audit inputs, not evidence for a future release candidate.

## Reproduce and check

```sh
./scripts/generate-notices.sh --cargo-about /path/to/cargo-about
./scripts/generate-notices.sh --check --target aarch64-apple-darwin --rust-release 1.100.0
./scripts/generate-notices.sh --release-check
python3 packaging/licenses/test_generate.py
```

Regeneration requires cargo-about **0.9.2**, Cargo, and access to the locked crate
sources. It may fetch dependencies. Obtain cargo-about from its
[official releases](https://github.com/EmbarkStudios/cargo-about/releases/tag/0.9.2)
and verify the release checksum before using it. The audit used the official
`cargo-about-0.9.2-aarch64-apple-darwin.tar.gz` archive with SHA-256
`ae72f0df0c399a1e96336f696fa55b1b28679fd725632eba8cf8e4568467cc3e`.
No global installation is required.

`--check` requires Python 3, the pinned Cargo toolchain, and repository files. It checks
content hashes for the artifact and its inputs, including the lockfile, workspace manifests,
`rust-toolchain.toml`, generator, configuration, supplemental notices, Rust
resources, and this audit. Success means
repository freshness and integrity only. It does not inspect the local Cargo
cache or re-read dependency files; regeneration performs those source checks.
Workspace manifests come from Cargo metadata (`--no-deps --offline --locked`),
including implicit local path members and honoring member patterns/exclusions.
Declared local path dependencies are inspected recursively, including excluded
packages and their transitive path dependencies. Exclusion from workspace
membership does not exclude a dependency's manifest from the freshness check.
This reads package metadata without compiling or downloading dependencies and
works with an empty registry cache. Unrelated local app directories do not
affect the inventory.
It does not review license obligations. Normal local packaging requires this
freshness check. `--release-check` additionally requires `reviewed.sha256` to
match the SHA-256 of the exact `manifest.json` bytes. That file records this
inventory review, not legal clearance or release authorization. Regeneration
never creates or updates it. After changed inputs are regenerated, inspect the
changed sources, selected terms, generated HTML, and runtime resources before
manually recording the new manifest hash. An old or missing review hash fails
even when freshness passes. Neither command publishes files.
Pass `--target <triple>` to assert the package target; targets other than
`aarch64-apple-darwin` fail until a corresponding inventory is implemented.
Packaging also checks `--rust-release` against the runtime notices, currently
1.100.0. It queries the compiler inside the build checkout and honors `RUSTC`.

The generator uses genuine source files selected by cargo-about. It omits
source-less fallback text and the detected MIT placeholder from the HTML, listing
the affected packages explicitly instead. Reviewed alternative selections in
`supplemental.json` must appear in the package's declared license expression;
their collected terms replace the tool's selection for those packages only.
Canonical terms are labeled separately from recovered upstream files.
The four original bundled font notices
are preserved in `supplemental.json` and the HTML, including the Hack notice that
contains Source Foundry, DejaVu, and Bitstream terms. Font byte lengths and hashes are recorded
and verified against the locked crate on regeneration. Checked-in upstream
supplements cite exact repository commits and retain the provided text unchanged.
Both regeneration and `--check` verify every stored notice's text hash, including
URL-backed records. Neither fetches those URLs again; changes to the supplements
require a new source audit. HTML escaping changes markup representation, not rendered text.

## Collected source coverage

The 2026-10-05 packaging follow-up limits manifest discovery to Cargo workspace
members and their declared local path dependencies. An unrelated local app had caused a false
stale-inventory failure. Regeneration with the verified cargo-about 0.9.2
binary produced byte-identical notice HTML; the dependency lockfile, member
manifests, supplemental notices, and runtime resources were unchanged. Only
the generator and this audit changed in the recorded inputs. Regressions cover
unrelated app directories, member globs/exclusions, missing member manifests,
and license-only changes in implicit members and excluded transitive path
dependencies. The dependency regressions use an empty
Cargo cache and verifies that both freshness and release checks reject stale
reviewed inputs. Cargo provides membership semantics directly, so a separate
Python TOML parser and Python 3.11 requirement are unnecessary.

The HTML's **Tool fallback inventory** records cargo-about's original missing
file/template selections. It is not the list of still-missing notices. The
supplemental section now contains 55 records with exact source identifiers and
text hashes. Source headers retain their original copyright/SPDX text.

| Area | Verified additions or existing coverage |
|---|---|
| AccessKit | Source copyright headers, kurbo derivation statements, root MIT/Apache files, and both pinned `LICENSE.chromium` versions cover the identified AccessKit/Chromium/kurbo omissions. |
| Older objc2/block2 | Full Steven Sheldon MIT notices from the locked `LICENSE.txt` files cover objc2 0.5.2, block2 0.5.1, and AppKit/Foundation 0.2.2. |
| Newer MIT-only objc2 | The MIT notice from verified ancestor `757cd841d0293341dbefd9b99d7548dec244ffde` is preserved alongside the locked licensing statements for objc2 0.6.4, block2 0.6.2, Foundation 0.3.2, and objc2-encode 4.1.0. The explicitly copied Rust OS-version helper also has its pinned Rust MIT notice. |
| ab_glyph/parser | Actual Apache files cover ab_glyph, its rasterizer, and owned_ttf_parser. The rasterizer's Google/Alex Butler source header and named stb upstream license are included. |
| egui/profiling/dpi | Existing egui/profiling root MIT texts were re-fetched at the locked commits and matched. DPI's Apache and libm MIT files were verified; its `std` feature is active. emath's named AHEasing UNLICENSE was added. |
| enum-map/derive | Their offered Apache file and all nine source copyright/SPDX headers were verified against locked archives and pinned Codeberg commits. No holder was invented for the generic MIT template. |
| Other COPYRIGHT files | core-graphics and utf8_iter COPYRIGHT files were added. Existing rustix and unicode-segmentation supplements remain included. The tiff test-only COPYRIGHT is outside this normal dependency scope. |

Exact commits, source URLs, and line ranges accompany the records in
[`supplemental.json`](supplemental.json). stb and AHEasing references name
projects without pinning the imported revisions. Their collected upstream
notices predate the adaptations; do not describe that as proof of exact import
history.

## Embedded fonts

All four complete font byte arrays from `epaint_default_fonts@0.33.3` were found
unchanged in the inspected executable. Its locked source commit is
`44cdd653e2317d300fb8a6c9c36b03f23991e803`.

Hack's source notice matches the Source Foundry MIT, DejaVu, and Bitstream terms
in its metadata. The emoji-icon-font notice retains John Slegers' copyright;
its font has no additional copyright field. Noto Emoji and Ubuntu Light have
copyright/trademark text in name-table records 0 and 7 that their OFL/UFL files
omit. Those exact decoded statements are now included, with source font and
field IDs recorded. Asset hashes bind them to the inspected versions.
Regeneration checks font length/hash; metadata extraction remains a review step
when a font changes.

## Rust runtime resources

The executable contains std, alloc, compiler_builtins, core Unicode, and
backtrace/addr2line symbols. Application Cargo metadata does not cover these.
Packages include seven files in `Contents/Resources/Licenses/Rust/`:

| Resource | Provenance |
|---|---|
| `COPYRIGHT-library.html` | Exact Rust 1.88.0 installed library notice, 376,423 bytes; SHA-256 `3d3f60160f5214efa0a7fd804102d02ce9ea6af04b5249a19eeb243450246ae9`. |
| `COPYRIGHT-backtrace.html` | Five unchanged sections from that toolchain's full `COPYRIGHT.html`, with a provenance wrapper: addr2line 0.24.2, adler2 2.0.0, memchr 2.7.4, miniz_oxide 0.8.8, object 0.36.7. |
| `LICENSE-MIT`, `LICENSE-APACHE` | Exact root files at Rust source `6b00bc3880198600130e1cf62b8f8a93494488cc`, including the actual Rust Project Contributors notice. |
| `Unicode-3.0.txt` | Exact toolchain permission text for the Unicode license named by core's Unicode-data source. |
| `compiler-builtins-libm-LICENSE.txt` | Referenced libm notice at compiler_builtins 0.1.158 source `a4c748f72a1dce652cc3e41c3a8425731bd1519a`. |
| `compiler-rt-CREDITS.TXT` | Referenced credits at Rust 1.88.0's LLVM submodule `c1118fdbb3024157df7f4cfe765f2b0b4339e8a2`. |

The [pinned Rust COPYRIGHT](https://github.com/rust-lang/rust/blob/6b00bc3880198600130e1cf62b8f8a93494488cc/COPYRIGHT)
defines the library document's scope. The five extra backtrace packages appear
in that revision's library lockfile and std feature path; their rlibs exist in
the sysroot. All five notices are included without asserting that every object
survived linking. The library document also covers other platforms; inclusion
does not mean every listed package is linked into Quarry.

The compiler-wide HTML is not packaged. Its SHA-256 is
`2bd426a843b7d2cc1c4f6600fbe4d15b90a1cb526e26c987b8b7165b98336de1`;
the five extracted sections were compared byte-for-byte. All seven packaged
resource hashes are recorded in [`manifest.json`](manifest.json).

`otool -L` showed only Apple system install paths. No framework or dylib is
redistributed in this app. Inspected native build outputs linked AppKit/libobjc
and supplied no additional static archive or generated resource requiring a
collected notice. Repeat these checks for a changed source, target, or toolchain.

## Resolution of the five recorded gaps

`dispatch@0.2.0` explicitly declares MIT in its
[pinned Cargo manifest](https://github.com/SSheldon/rust-dispatch/blob/82d6c7a5b75dc0c71c3f46f87bb6c16a476f7748/Cargo.toml).
The complete 14-file tree supplied no standalone copyright or permission
statement. The original published manifest and all six Rust files match that
revision. Cargo defines the manifest's `license` field as the license under
which the package is released and accepts either it or `license-file`, as
documented in the [Cargo reference](https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields).
The inventory now preserves that exact declaration and the canonical MIT
permission terms. It retains Steven Sheldon as manifest authorship metadata,
without inventing a copyright statement or year. The canonical terms are not
presented as a recovered upstream file. This closes the source-collection gap;
it does not establish sole authorship or make a legal conclusion about contributors.

For `dispatch2@0.3.1`, `objc2-app-kit@0.3.2`,
`objc2-core-foundation@0.3.2`, and `objc2-core-graphics@0.3.2`, this inventory
selects their offered **Zlib** alternative. All four locked manifests declare
`Zlib OR Apache-2.0 OR MIT`. The pinned root licensing statements at
[dispatch2's revision](https://github.com/madsmtm/objc2/blob/8852b424193ca41602281b3d7540d7c8ed51e49a/LICENSE.md)
and [the framework revision](https://github.com/madsmtm/objc2/blob/7b1abfd750a2cacaea71d6a56ecfb83cb7de560b/LICENSE.md)
explicitly allow that choice. All 472 files across their four published archives
were compared with the local sources, and archive hashes match `Cargo.lock`.
The [Zlib terms](https://zlib.net/zlib_license.html) make product acknowledgment
optional and require notice retention in source distributions. The app contains
unchanged compiled dependencies, not their sources. The inventory nevertheless
retains the collected statements and canonical Zlib terms. It does not attribute
objc2 to the authors of the unrelated zlib compression library. MIT-only objc2,
block2, Foundation, and objc2-encode retain their collected MIT notices.

The canonical MIT permission terms and complete Zlib text come from the
[pinned SPDX license data](https://github.com/spdx/license-list-data/tree/3ac5a9c241d97f95b22a5e366c9c841404a35639/text).
The MIT template's generic copyright line is excluded, not filled in. Exact
source ranges and content hashes accompany both records in `supplemental.json`.

The newer objc2 explanation discusses Apple SDK-derived code. The inspected
API/ABI links alone do not establish another missing copied-code notice; no
Apple permission file was present to collect. Do not invent a copyright holder
or assert a new obligation from that observation.

Recheck the final binary and keep candidate, supported-platform, signing, and
owner-acceptance gates separate in the [beta checklist](../../docs/BETA_RELEASE_CHECKLIST.md).
This recorded review covers the specified inventory only. A changed dependency,
toolchain, native resource, or target requires checking the affected provenance.

## 2026-10-19 private-storage follow-up

The Unix owner check enables the existing Rustix `process` feature. Regeneration
with the checksum-verified cargo-about 0.9.2 ARM64 tool produced byte-identical
notice HTML (SHA-256
`6a816f8e7da6ec6ff2fed36f8a666c1d6eeab062ed927aaf3915a8da5615a3e3`).
The lockfile, selected notice terms, supplemental text/font hashes, and Rust
runtime resources are unchanged. The affected inputs are the core manifest and
this audit record. The regenerated manifest hash records this bounded source
and inventory review; it is not a change of license terms or release approval.

## 2026-11-20 compressed delimited files

Gzip support ([ADR 0012](../../docs/adr/0012-compressed-delimited-files.md))
makes `quarry-core` depend directly on `flate2@1.1.9` with only its pure-Rust
`miniz_oxide` backend. `flate2@1.1.9`, `miniz_oxide@0.8.9`, and
`crc32fast@1.5.0` were already locked and listed through image dependencies,
so no crate was added to `Cargo.lock`. `quarry-cli` and `quarry-egui` use the
same `flate2` only as a development dependency for tests. Regeneration with the
checksum-verified cargo-about 0.9.2 ARM64 tool produced byte-identical notice
HTML (SHA-256
`6a816f8e7da6ec6ff2fed36f8a666c1d6eeab062ed927aaf3915a8da5615a3e3`). The
affected inputs are the lockfile's dependency lists, the core, CLI, and egui
manifests, and this audit record. The regenerated manifest hash records this
bounded review; it is not a change of license terms or release approval.

## 2026-11-22 frontend-neutral quarry-workbench extraction

Extracting Quarry's UI-independent document model from `apps/quarry-egui` into
the new first-party workspace crate `crates/quarry-workbench` (`MIT OR
Apache-2.0`) adds zero third-party crates to `Cargo.lock`: `quarry-workbench`
depends only on internal `quarry-core` and already-locked `tempfile@3.27.0`
(with already-locked `flate2@1.1.9` as a dev-dependency only). Regeneration
with the checksum-verified cargo-about 0.9.2 ARM64 tool updated only the locked
package count (142 to 143) and added the single `quarry-workbench@0.1.0`
(`MIT OR Apache-2.0`) row in `THIRD_PARTY_NOTICES.html`. Selected third-party
notice terms, supplemental text/font hashes, and Rust runtime resources are
unchanged. The affected inputs are `Cargo.toml`, `Cargo.lock`,
`apps/quarry-egui/Cargo.toml`, `crates/quarry-workbench/Cargo.toml`, and this
audit record. The regenerated manifest hash records this bounded review; it is
not a change of license terms or release approval.

## 2026-11-22 Rust 1.100.0 runtime notices and GPUI workspace unification

Unifying `apps/quarry-gpui` (on `gpui-kit = "=0.10.0"`) into the root Cargo
workspace requires upgrading the pinned workspace toolchain from Rust 1.88.0 to
Rust 1.100.0 (`0ef1bc386059596dada8fb65b50425aab2ea637a`, LLVM 23.1.5, release
dated 2026-11-12). `apps/quarry-egui` remains the packaged macOS `.app` target
until the Phase 4B-3 cutover, so `THIRD_PARTY_NOTICES.html` continues to
inventory `apps/quarry-egui/Cargo.toml` for `aarch64-apple-darwin`.

Updated Rust 1.100.0 runtime resources in `packaging/licenses/rust/`:

| Resource | Provenance |
|---|---|
| `COPYRIGHT-library.html` | Exact Rust 1.100.0 installed library notice, 1,512,515 bytes; SHA-256 `626556344c12679d7cab2c1aa2ce80de01937b62c2c88c62f0650a469be18799`. |
| `COPYRIGHT-backtrace.html` | Five unmodified sections from Rust 1.100.0's full `COPYRIGHT.html` (SHA-256 `d5a611d33c23248dec086ca86ce6123ecfc26b2b9666e34beaa4dda012d53e4b`, 58,597 bytes, SHA-256 `3f75bc40ac42851491dcb65fd6f18c258325578e9f3efa63391a9eb682628461`): `addr2line-0.25.1`, `adler2-2.0.1`, `memchr-2.8.2`, `miniz_oxide-0.8.9`, and `object-0.39.1`. `library/Cargo.lock` at `0ef1bc386059596dada8fb65b50425aab2ea637a` pins `addr2line@0.27.1`, `adler2@2.0.1`, `memchr@2.8.3`, `miniz_oxide@0.9.1`, and `object@0.39.1`; the license files in the checksum-verified crates.io archives for `0.27.1`, `2.8.3`, and `0.9.1` were compared byte-for-byte against those extracted sections and match identically. |
| `LICENSE-MIT`, `LICENSE-APACHE`, `Unicode-3.0.txt`, `compiler-builtins-libm-LICENSE.txt`, `compiler-rt-CREDITS.TXT` | Verified byte-for-byte unchanged against Rust 1.100.0 (`0ef1bc386059596dada8fb65b50425aab2ea637a`), `library/compiler-builtins/libm/LICENSE.txt`, and LLVM 23.1.5 submodule `0a5579eb649d3634a9c30ac873300cc2944fa070`. |

Regenerating `THIRD_PARTY_NOTICES.html` with the checksum-verified cargo-about
0.9.2 ARM64 binary updates the runtime notice prose from Rust 1.88.0 to Rust
1.100.0 and reflects the unified workspace lockfile resolution in the
`quarry-egui` graph (143 to 144 locked packages: adding `synstructure@0.14.0`
under `MIT`, and minor/patch bumps across the `Unicode-3.0`-licensed ICU4X
crates `icu_*@2.4.0`, `tinystr@0.8.5`, `yoke@0.8.4`, `yoke-derive@0.8.4`,
`zerovec@0.11.9`, and `zerovec-derive@0.11.7`). Selected third-party license
terms and supplemental text/font hashes are unchanged.

Read-only pre-audit of `apps/quarry-gpui/Cargo.toml` for `aarch64-apple-darwin`
against the current `about.toml` (ahead of the Phase 4B-3 packaging switch)
flagged 11 crates using five OSI-approved licenses not yet listed in the
`quarry-egui` allowlist: `0BSD` (`enum-iterator@2.3.0`,
`enum-iterator-derive@1.5.0`), `BSD-2-Clause` (`arrayref@0.3.9`),
`BSD-3-Clause` (`encoding_rs@0.8.44`, `exr@1.74.3`, `instant@0.1.13`,
`lebe@0.5.3`, `tiny-skia@0.11.4`, `tiny-skia-path@0.11.4`), `CC0-1.0`
(`notify@8.2.2`), and weak file-level copyleft `MPL-2.0` (`option-ext@0.2.0`
via `dirs-sys`/`zed-font-kit`). Zero GPL, LGPL, AGPL, or non-OSI dependencies
were found. A full `quarry-gpui` notice inventory and supplemental review will
be generated before the Phase 4B-3 packaging cutover.

The affected inputs are `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`,
`apps/quarry-gpui/Cargo.toml`, `packaging/licenses/generate.py`,
`packaging/licenses/rust/COPYRIGHT-library.html`,
`packaging/licenses/rust/COPYRIGHT-backtrace.html`, and this audit record. The
regenerated manifest hash records this bounded review; it is not a change of
license terms or release approval.
