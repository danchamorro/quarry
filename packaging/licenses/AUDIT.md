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
./scripts/generate-notices.sh --check --target aarch64-apple-darwin --rust-release 1.88.0
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

`--check` requires only Python 3 and repository files. It checks content hashes
for the artifact and its inputs, including the lockfile, workspace manifests,
`rust-toolchain.toml`, generator, configuration, supplemental notices, Rust
resources, and this audit. Success means
repository freshness and integrity only. It does not inspect the local Cargo
cache or re-read dependency files; regeneration performs those source checks.
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
1.88.0. It queries the compiler inside the build checkout and honors `RUSTC`.

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
