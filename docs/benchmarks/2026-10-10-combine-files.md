# Combine matching files

Validation for [issue #54](https://github.com/danchamorro/quarry/issues/54),
recorded October 10, 2026. Native interaction checks ran through VMPal in the
macOS guest. No host-desktop E2E tests were performed.

## Candidate and environment

| Field | Value |
|---|---|
| Application | Main egui application, local development package |
| Version/build | 0.1.0 (145), arm64 |
| Source | `34145e61ed146b35bd1ed899a63d72b3be845ccc`, clean |
| Guest | VMPal macOS 27.0.1 (26A434), 8 virtual CPUs, 16 GiB RAM |
| Build | Release, packaged with `scripts/macos-app.sh package` |
| Package SHA-256 | `3937edce21eb1e41f0caf5fc66d873cc0151986ce5ec8f3d84e7b6d0fa0f6947` |

The ZIP checksum matched in the host and guest. The package passed bundle
verification and guest `codesign --verify --deep --strict`; its revision and
clean source status were verified before it launched successfully. This was an
ad-hoc-signed development package, not a published or notarized release. It ran
from a separate guest test directory without replacing either installed app.

## Native workflow checks

- Selected ten CSV files through the native picker, including adding files to
  an existing selection. Changed their order with **Down**, checked all ten,
  reviewed the 3-column/30-row summary, and saved a new output.
- The 617-byte output matched an independently constructed expected file
  byte for byte. The order was files 1, 3, 2, then 4 through 10. Each source
  contained a quoted comma, an embedded newline, Unicode, CRLF records and an
  unterminated last record; one source also began with a UTF-8 BOM. Duplicated
  rows were retained and the output contained one header. Opening the result
  displayed all 30 rows and 3 columns.
- Two headerless files produced all 3 data rows, including both first rows and
  the correctly separated unterminated boundary. The output matched the
  independent 23-byte expected file exactly.
- Made an unsaved cell edit in the open test document, then combined the
  headerless files. The edit remained visible and marked **Modified (not
  saved)**. **Open Combined File** explained that the open document must first
  be saved or discarded; it did not replace it. The original on-disk test
  document still matched its pre-edit bytes.
- Mismatched headers failed before any output selection, identifying the
  second file, record 1, and the differing header column. A selected checksum
  file with incompatible width was also rejected with file/record context.
- Resized the native app to a compact window. The error wrapped, and **Close**
  and **Check Files** remained reachable. Cancelling the native Add Files
  picker preserved the selection; Escape then closed the idle dialog.
- All small source fixture hashes remained unchanged.

## Large output and cancellation

The completed run combined two 1,039,500,015-byte CSVs, each containing
5,500,000 data rows. A row contains a quoted comma and a 170-character repeated
payload. An independent shell stream copied the first input and omitted the
second input's header. Its full SHA-256 matched the application's output.

| Check | Result |
|---|---|
| Output rows | 11,000,000 |
| Output bytes | 2,079,000,015 |
| Each input SHA-256, before and after | `7dd556bf8958d231ff31fbbabcaaf4d375087af3f12b56be8284200b3c7357eb` |
| Expected and actual output SHA-256 | `9adf8031a29ade6d46e3f02f3cc5fc6b40d599f0ca4b3133d275e7adaad82f0b` |
| Maximum sampled application RSS | 169,872 KiB (165.9 MiB) |

A separate run validated two 10,395,000,150-byte inputs, then began writing a
20,790,000,285-byte output. These inputs concatenate ten copies of the above
fixture, so repeated header-shaped rows are ordinary data. The UI reported
110,000,018 data rows. **Cancel** was clicked while the writer displayed 5%
progress on the first file. The dialog reported cancellation, the destination
did not exist, and the output directory contained only the two source files:
no staging file remained. Both input hashes remained
`c0e5360aaf5a119b151d381f202b9c9c456daae60a20c1d084d8dd8226eaf1fd`.
Maximum sampled application RSS during this write/cancel interval was
168,720 KiB (164.8 MiB).

RSS sampling used guest `ps` approximately every 100 ms. These are single runs
with warm, uncontrolled caches and repetitive synthetic data. They demonstrate
bounded memory on these inputs and cancellation during writing; they are not
general throughput measurements or a claim about every possible record shape.

## Automated checks

Formatting, workspace Clippy with warnings denied, all 336 workspace tests,
and the release workspace build passed. Focused regressions cover:

- Exact ordered output for multiple inputs, duplicates, multiline quoting,
  Unicode, BOMs, CRLF, headerless and header-only files.
- Header order/case/space differences, late ragged records, malformed data,
  mixed delimiters, empty files and repeated paths.
- Changed sources, existing destinations, input-as-output, publication-time
  source checks, deterministic cancellation and staging cleanup.
- Records crossing read chunks and worker panic completion reporting.
- Compact footer/accessibility controls and preserving an unsaved document
  throughout a successful combination and attempted result open.

Screenshots, build/test logs, package, fixtures and guest memory/checksum
evidence are retained locally under `target/combine-files/`. The existing host
installation was not updated by this feature-validation run.
