# Save As after column reordering

Validation for [issue #90](https://github.com/danchamorro/quarry/issues/90),
recorded October 10, 2026. Native interaction checks ran in VMPal, not on the
host desktop.

## Candidate and environment

| Field | Value |
|---|---|
| Application | Main egui application, local development package |
| Version/build | 0.1.0 (144), arm64 |
| Source | `7babb1b6756652a808e0b8619581cae7985f0f72`, clean |
| Guest | VMPal macOS 27.0.1 (26A434), 8 virtual CPUs, 16 GiB RAM |
| Host | Apple M3 Max, 128 GiB RAM; Rust 1.88.0 |
| Build | Release, packaged with `scripts/macos-app.sh package` |
| Package SHA-256 | `aaeca7a3344f6f3018a2473756d90eda866190d4623f61a964b5148c78f685a2` |

The package passed bundle verification on the host and `codesign --verify
--deep --strict` in the guest. Its source revision and clean status were checked
in the guest before launch. It is an ad-hoc-signed development build, not a
published or notarized release. It ran from a separate guest test directory;
neither the guest's installed beta nor the host's installed app was replaced.

## Native workflow checks

- Reproduced the original behavior in installed beta 2, build 140: dragging
  Amount before Name in **Columns…** left **Save As…** disabled.
- In the candidate, the same drag enabled **Save As…** without editing a cell or
  header. Cancelling the native save picker retained the layout and allowed a
  subsequent save.
- Saved and reopened a three-column CSV containing a quoted comma, an embedded
  newline, CRLF record endings and a blank field. A byte comparison against an
  independently constructed expected file passed. Headers and values were in
  `ID,Amount,Name` order.
- Hid Name, reordered Amount, changed the first amount from 10 to 11, and saved
  another copy. The output retained Name and its complete values, included the
  edit, and reopened with all three columns.
- Used **Move Selected Columns…** to move Amount to position 2 and then saved
  without a cell or header edit. The output matched the expected file exactly.
- The original small fixture's SHA-256 remained
  `0d07d3f5db5792b86011838430e2fc50399f71a711a0b4ea7b6dbdcca5a1837b`.

## Large-file save and cancellation

The completed-save fixture contained 5,500,000 data rows and 1,039,500,015
bytes. Each row had three fields, including a quoted comma and a 170-character
repeated payload. The output was saved through the native Save As picker and
storage review after dragging Amount before Name. Its full SHA-256 matched the
expected transformed stream; the source hash remained unchanged.

| Check | Result |
|---|---|
| Source SHA-256 | `7dd556bf8958d231ff31fbbabcaaf4d375087af3f12b56be8284200b3c7357eb` |
| Expected and actual output SHA-256 | `e9aaaa240f4aac111a431321f4c4f989a4a11a2f7449468f13af8f17a8f0e056` |
| Observed staging-to-publication interval | 2.31 seconds |
| Maximum sampled application RSS | 203,072 KiB (198.3 MiB) |

A separate 10,395,000,150-byte fixture concatenated ten copies of the input
(including the repeated header records). After the native storage review,
**Cancel Save As** cancelled the active background save during preparation.
The reordered document remained available, the destination did not exist,
and no `.quarry-export-*` staging file remained. The input's before/after
SHA-256 matched:
`c0e5360aaf5a119b151d381f202b9c9c456daae60a20c1d084d8dd8226eaf1fd`.

Timing and RSS were sampled with guest `ps` and a `Time::HiRes` monitor at
approximately 100 ms intervals. The interval starts when staging first appears
and excludes the save picker and storage review. This is one local run with
uncontrolled warm caches and highly repetitive synthetic data, not a cold-cache
benchmark or a general throughput claim. Cancellation during preparation does
not independently validate cancellation after output bytes have been written.

## Automated checks

Formatting, workspace Clippy with warnings denied, all 323 workspace tests,
and the release workspace build passed. Focused regressions cover:

- Accessible Save As availability after reordering and reset.
- Headered and headerless output, pending cell edits, header edits, hidden
  columns, quoted values, embedded newlines, CRLF and reopening.
- Late ragged fields beyond the discovered width, and padding short records.
- Picker cancellation, an existing destination, source-change blocking, and
  structural moves combined with view reordering.
- Storage allowance for padding every ragged row without changing ordinary
  Save's allowance.

The existing engine regressions also passed for cancellation, write failure,
source conflicts and no-clobber publication. Raw screenshots, build metadata,
test output and memory samples are retained locally under
`target/vmpal-issue90/`.
