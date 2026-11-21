# Compressed delimited files: acceptance evidence

This report records the release benchmark and native app checks for
[#69](https://github.com/danchamorro/quarry/issues/69) on branch
`feat/compressed-delimited-files`. It does not claim that review, merge or the
installed VM update has happened.

All work ran in VMPal macOS VM 1: macOS 27.2.1 (26B131), Apple M3 Max
(Virtual), eight virtual CPUs, 16 GiB RAM (17,179,869,184 bytes), Rust 1.88.0.
Every file was synthetic. The host Mac and the VM's installed
`/Applications/Quarry.app` were not changed.

## Source and checks

- Engine: 5927d10bdc28d2b3e45a73ca1bf254ab4f37ebee. App and CLI:
  1f0cbbf31ae4ba856bf5e3463863bcffc78c7cfa.
- Measured and packaged revision, with the probe and harness:
  6bfbab75ea4e6b2aa3c2722cb5c2268e20ffe9f0, clean.
- At that revision `cargo fmt --check`, all-target/all-feature Clippy with
  warnings denied, and `cargo test --workspace --locked` pass (736 tests,
  including 395 core and 290 egui tests; none failed).
  `generate-notices.sh --release-check`, all 17 `macos-app.sh self-test`
  checks and `macos-app.sh package` pass.

## Release measurements

The [release probe](../../crates/quarry-core/examples/compressed_acceptance.rs)
calls the public `CompressedBootstrap`, `CompressedDecompressJob`,
`Session::read_rows`, `start_filtered_export` and `start_save_as_with_edits`
APIs. The [harness](../../scripts/bench-compressed-acceptance.py) builds it from
the clean checkout, writes the fixtures, computes expected SHA-256 values
without Quarry, runs the probe under `/usr/bin/time -l` while sampling
`du -sk`, and checks every result. Probe binary SHA-256:
`ae690a92514ebeab51a6cc939d39716218774703306815e6a86969492ffd1615`.
[Machine-readable results](2026-11-20-compressed-files.json) keep every count,
byte total, hash and timing.

| Case | Compressed bytes | Uncompressed bytes | Outcome | Peak RSS bytes |
|---|---:|---:|---|---:|
| large | 504,036,953 | 19,327,352,853 | complete; all checks pass | 50,446,336 |
| cancel | 504,036,953 | stopped at 549,453,824 | cancelled; cleanup in 0.026 s | 50,315,264 |
| limit | 656,480 | 335,544,329 | `DecompressionLimitExceeded` at 67,108,864 | 7,749,632 |
| corrupt | 973–1,036 | 7,507 | three `CompressedFile` rejections; valid control opens | 2,572,288–2,883,584 |

### Larger than RAM

The fixture is one `.csv.gz` with two gzip members. They hold 9,663,741,973 and
9,663,610,880 uncompressed bytes, so each member's ISIZE field wraps modulo
2^32 (1,073,807,381 and 1,073,676,288). The member boundary falls inside data
row 73,728. The 18.0 GiB stream exceeds the VM's 16 GiB RAM. It has a header
and 147,456 records of 128 KiB. Every record has a quoted name with a comma
and Unicode. Record 12,345 has an embedded newline and doubled quotes. Every
10,000th record contains `TARGET-MATCH`. Generation took 11.9 s.

| Step | Result |
|---|---|
| First rows (bootstrap) | 100 rows in 10.1 ms after reading 262,144 compressed bytes |
| Read while decompressing | record 32 read in 2.6 ms with 360 records indexed |
| Decompress and index | 13.18 s; 147,457 records, 4 columns, 19,327,352,853 bytes |
| Viewport reads | records 0, 1, 73,728 and 147,456 return the expected ids; slowest 1.1 ms |
| Filtered export | 15 rows, 1,966,101 bytes in 5.27 s; SHA-256 matches |
| Uncompressed copy | 19,327,352,853 bytes in 13.22 s; SHA-256 matches |
| `.csv.gz` destination | refused (`InvalidOption`) |
| Source and residue | source SHA-256 unchanged; no `.quarry-*` entries remain |

Expected SHA-256 values: uncompressed
`03644ea905c2c7fcb42fcda445803533600a75875eed9456fa9654aad8303a79`, filtered
`a9730edaa0b4807f4385bf5461464153cca7173696b131b49766189b2b7b1b61`, source
`f8279395d496c02676827a5c16ef62c7970cf4945afcdaaeb6c5b8237c6e8ca5`.

The whole probe took 32.1 s. Sampled disk use peaked at 38,799,097,856 bytes in
the fixture folder: the source, the private copy and the uncompressed copy
together. The private copy alone reached 19,328,401,408 bytes. As ADR 0012
states, opening a compressed file needs free space for its full uncompressed
size.

### Cancel, limit and corrupt input

- **Cancel.** The probe cancelled the large fixture once 549,453,824 bytes were
  written, after a 536,870,912-byte threshold. `wait` returned 0.026 s after the
  request, with the private copy already removed. The source hash was unchanged.
- **Limit.** A 656,480-byte gzip expands to 335,544,329 bytes of 1 KiB rows.
  With a 64 MiB limit the bootstrap succeeds and the on-disk pass stops with
  `DecompressionLimitExceeded`. The probe saw at most 67,108,864 bytes written.
  The message reports 68,157,440 decoded bytes, which were refused before being
  written. The private copy existed for about 0.05 s, so `du` samples every
  10 ms recorded 0 bytes; the probe's own count is the evidence here.
- **Corrupt.** A truncated stream, an inverted CRC-32 byte and trailing
  non-gzip bytes each fail during bootstrap with `CompressedFile` and a
  specific message. The undamaged two-member control opens with 201 records and
  3 columns. No private directory remains in any case.

RSS is the process maximum from `/usr/bin/time -l`, not a guaranteed memory
limit. `du` samples can miss short peaks. These are synthetic acceptance
measurements on one VM, not performance guarantees for other disks or record
shapes. No build or test ran during the timed cases.

Reproduce in the VM from a clean checkout. Each `--root` must not exist yet
(the cancel case reuses the large fixture):

```sh
mkdir -p /tmp/q69
python3 scripts/bench-compressed-acceptance.py --case large --root /tmp/q69/large
python3 scripts/bench-compressed-acceptance.py --case cancel --root /tmp/q69/large --reuse
python3 scripts/bench-compressed-acceptance.py --case limit --root /tmp/q69/limit
python3 scripts/bench-compressed-acceptance.py --case corrupt --root /tmp/q69/corrupt
rm -rf /tmp/q69
```

Add `--smoke` for small fixtures that check the harness in seconds.

## Native app checks

Packaged `target/package/Quarry.app`, build 167, clean source 6bfbab7,
executable SHA-256
`2f2b93a50070926942ff73d1ccad97b7a3d9628139cc57a029e76bfb4c29bcf2`. The VM's
installed Quarry was open with one unchanged document, so it was quit first.
The packaged app was opened with `open -a` and driven with VMPal input.

`sample.csv.gz` has two gzip members, a header and 500 rows. It has quoted
commas, Unicode, a multiline cell, and a unique value in each member.
`corrupt.csv.gz` is the same file with one CRC-32 byte inverted.

1. **Open.** 500 rows and 4 columns, index complete. The footer reads
   `Gzip (read-only) · 3.73 KiB → 21.09 KiB`. Rows 1–29 (member 1) and 472–500
   (member 2) display correctly. The multiline cell shows as
   `first line[LF]second line`.
2. **Read-only.** In the File menu, Save is disabled and Save As… is enabled.
   Save As UTF-8… is also disabled. Double-clicking
   a cell selects it without opening an editor, and typing changes nothing.
   Replace All is refused with "Compressed files open read-only. Use Save As…
   to create an editable uncompressed copy."
3. **Find.** Cmd-F for the member-2 value `MEMBER2-UNIQUE-ZEBRA` reports
   "Found row 420, column 3."
4. **Filters.** `city` contains `rich` gives "250 matching rows" and "Filter
   complete. 250 matches."
5. **Save As.** The panel is titled "Save uncompressed copy as" and suggests
   `sample.csv`. The saved `sample-out.csv` (21,595 bytes) is identical to
   `gunzip -c sample.csv.gz`, and the `.gz` hash is unchanged. The tab then
   shows the uncompressed copy, and the private directory is removed.
6. **Corrupt file.** Opening `corrupt.csv.gz` shows "The gzip CRC-32 checksum
   does not match the decompressed data; the file is corrupt." No tab opens.
7. **Quit.** Cmd-Q quits without a prompt. No Quarry process and no
   `.quarry-compressed-*` entry remain in `$TMPDIR` or `/tmp`.

Observations:

- The corrupt-file notice prints the path twice:
  `<path>: <path>: The gzip CRC-32 checksum …`. The message is otherwise
  correct. This is worth a small follow-up.
- CJK text (東京) draws as empty boxes in the gzip tab and in a plain CSV tab.
  The accessibility value is correct. This is font coverage, not
  decompression.
- The first Save As attempt produced `sample-out.csv.csv`, because the typed
  name replaced only the preselected stem. Quarry had suggested `sample.csv`.
  That file's bytes also matched. The second attempt saved `sample-out.csv`.

## Remaining delivery gates

The branch still needs a pull request, a completed CodeRabbit review of its
latest commit, resolved findings and green required CI before merge. After
merge, the VM's `main` checkout must be updated and the app installed with
`./scripts/macos-app.sh install`. `verify` must then show the merged revision
and a clean source status, and the reopened app must be checked.
