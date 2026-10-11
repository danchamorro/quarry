# Row and column insertion validation — 2026-10-10

Implementation: c6b814ec329c9cb5dedd68bdaa9f85954e260a18, main egui app and shared
engine, [issue #73](https://github.com/danchamorro/quarry/issues/73).
The report itself is a documentation follow-up.

## Environment

- Configured VMPal macOS VM: macOS 27.0.1 (26A434), arm64, 8 vCPUs, 16 GiB RAM.
  Host CPU: Apple M3 Max. Source changes, builds, review and app interaction
  occurred in the VM.
- Rust 1.88.0, locked dependencies, optimized release build with workspace LTO.
- Packaged app 0.1.0, build 148, revision above, QuarrySourceStatus=clean.
  Strict guest signature verification and actual launch passed; Gatekeeper
  protections were unchanged.
- Executable SHA-256:
  a3e81d665cbdc469132ddb3f3473b21f54132ecf7ba0c6e3c7cefad1a55688ea.
- Synthetic fixtures under /tmp/quarry-insertion-review; ignored logs in
  target/insertion. No host app work, GPUI work, installed-app update or public
  release is included.

## Automated correctness

All **378 workspace tests** passed: 183 core, 154 egui, 31 CLI, 9 delimited
and 1 AppKit. Formatting, strict workspace Clippy, release build, packaging
self-tests and package verification passed.

New regressions cover first/middle/final row boundaries, CRLF/LF, unterminated
records, literal trailing CR, BOM relocation and BOM-only files, empty/header-only/
headerless files, ragged padding, late extra fields, quoted names, multiline
values, Unicode and pending cell/header edits. Tiny read chunks exercise scanner
boundaries. Invalid counts/positions, width/header mismatch and record-size
overflow fail without publication. Added separators obey the record limit.
Cancellation and changed sources preserve source bytes and clean private output.

Desktop checks cover accessible fields, disabled invalid submission, all four
accessible context-menu commands and their exact physical boundaries, hidden/
reordered columns, filter/index restrictions, editing new cells, pending values,
structural Undo/Redo and exact Save As output. Empty/header-only tests select
explicit header modes.

## Native VMPal workflow

The clean packaged app launched in the guest with a synthetic ID/Note file,
numeric IDs, a quoted comma, multiline data and Unicode.

- Edited an existing value without saving, then right-clicked data row 2 and
  inserted two rows above it. Four rows appeared and the pending value survived.
- Undo restored the two original data rows and retained the pending value;
  Redo restored the blanks. Entered “new” into an inserted cell.
- Right-clicked column 2 and inserted two columns before it. Named/renamed the
  first header to “New,Name”, edited an inserted cell to “inserted, value”,
  then used native Save. Full bytes matched an independent oracle with quoting,
  blank rows, multiline data and Unicode intact.
- Used **Insert → Rows…** to append another blank row, then native **Save As…**
  to the panel's default native-edited.csv. Full bytes matched a second oracle;
  the previously saved source retained its exact bytes.
- Closed and reopened the result successfully. Auto header detection interpreted
  this mixed-text/blank-header fixture as headerless; its header appeared as the
  first data record. Explicit header modes are covered in automated checks;
  reopening preserved all bytes.
- Requested 10 billion blank rows to keep cancellation running. The storage
  dialog confirmed capacity before **Continue**. Clicked footer **Cancel Change**;
  it reported “Row insertion cancelled. The document was not changed.”
  Saved bytes were unchanged and no working/export artifact remained after close.
- Inserted another row and chose **Discard Changes**. The prior row count and
  exact saved bytes returned. Quit cleanly; no test app process remained.

VMPal screenshots/input drove native controls. Explicit mouse-down/up holds
were used when the shortest click events were missed.

## Large-file measurements and independent oracle

The existing date-time-sort-fixture.py large fixture contains 5,000,000 data
rows and **1,155,000,013 bytes (1.08 GiB)**: three columns with dates, ordinal
IDs and quoted/multiline Unicode payloads. The public-engine API driver excludes
the desktop UI and its index/history from memory measurements.

~~~bash
python3 scripts/date-time-sort-fixture.py /NEW/input --large-rows 5000000
cargo build -p quarry-core --release --locked --example insertion-benchmark
/usr/bin/time -l target/release/examples/insertion-benchmark /NEW/input/large.csv /NEW/baseline.csv baseline
/usr/bin/time -l target/release/examples/insertion-benchmark /NEW/input/large.csv /NEW/rows.csv rows
/usr/bin/time -l target/release/examples/insertion-benchmark /NEW/input/large.csv /NEW/columns.csv columns
python3 scripts/row-column-insertion-oracle.py /NEW/input/large.csv /NEW/rows.csv /NEW/columns.csv
~~~

Each measurement is one VM run with warm/mixed cache after prior reads;
caches were not purged. Baseline is a same-build unchanged Save As.
Worker times include the count pass and private-output publication.

| Operation | Worker time | Whole driver time | Peak RSS | Output bytes |
|---|---:|---:|---:|---:|
| Unchanged Save As | 0.889 s | 0.97 s | 4,243,456 (4.05 MiB) | 1,155,000,013 |
| Append 10 blank rows | 1.365 s | 1.36 s | 4,276,224 (4.08 MiB) | 1,155,000,043 |
| Insert two columns after column 1 | 3.134 s | 3.13 s | 4,456,448 (4.25 MiB) | 1,165,000,018 |

Both full output hashes matched independent Python oracles. The row oracle hashes
source bytes plus ten blank records; the column oracle uses Python CSV parsing/
serialization and hashes every transformed record. It checks the complete
record count and three-field source shape with bounded buffers.

- Source SHA-256: 8c6dc50423d01c292d96a82311ec7f3534e9d5881e212a0ac686234423e9c471
- Rows SHA-256: 85e6553d1aa88fb67f835f29c802d5ce8573fdc2934422c1843075d920120868
- Columns SHA-256: fdebaddb2795f25f87b5028e6bc2185809148286e973b7aed4fe3eab9165cd61

Row output has 5,000,010 data rows; column output has 5,000,000 data rows and
five columns, with one New header and one blank header inserted.

The driver's cancel-rows/cancel-columns modes cancel after observing at least
1 MiB of output. Polling overshot to 4,776,598 bytes for rows and 1,057,600 bytes
for columns. Both returned Cancelled, published no destination and left no
staging artifact; source bytes stayed unchanged. These are cleanup checks,
not cancellation-latency measurements.

The fixture is smaller than VM RAM. This validates bounded behavior for this
workload, without a new 50 GB or cold-cache claim. Output size governs temporary
disk use; existing preflight accounts for working output and retained history.


## Review follow-up

The review follow-up exposes separate counting progress, a conservative output
allowance and written-byte progress, including while blank rows precede source
copying. New engine/UI regressions bring the passing workspace total to **380**
(184 core, 155 egui). The empty forced-header message now describes an empty file.

A subsequent CodeRabbit review found that unchanged source records were still
entering the bounded rewrite buffer during row insertion. The copy path now
streams them directly and buffers only pending edits. A regression uses a small
configured limit with oversized quoted/multiline records, CRLF, BOM, terminated
and unterminated sources, insertion before/between/after records, and chunk sizes
1/2/7/32. Exact output is checked; editing an oversized record still fails safely
without publishing output. The existing appended-separator limit remains tested.
All 381 workspace tests, strict workspace Clippy and formatting passed after
this streaming correction inside VMPal.
