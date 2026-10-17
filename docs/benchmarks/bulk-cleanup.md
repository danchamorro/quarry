# Bulk cleanup validation

Validated in the VMPal macOS VM on 2026-10-16/17 for issues #96–#100.
The implementation was based on `a968121933c456c6eb836e0b0824957d2a3bd9e8`.
This report accompanies the implementation commit; these measurements used its
working tree before commit. All application builds and tests ran in the VM.

## Environment and fixture

- macOS 27.0.1 (26A434), Apple Silicon, 16 GiB guest RAM.
- Release core benchmark and packaged egui application.
- Synthetic CSV: 18,253,611,032 bytes (17 GiB plus header), 4,456,448 data rows.
- Header: `name,word,empty,padding\n`.
- Each 4,096-byte data row contains `  Alice  ,Straße,,`, ASCII `x` padding,
  and LF. The input exceeds guest RAM.
- Input SHA-256: `d6a99c2a48cabee2ff0a257714c82710c6d4206ef5e780503146de7d509919f8`.

## Full-file core results

Each job changed exactly 4,456,448 cells. An independent Python oracle generated
the expected repeated rows and streamed the full expected hash; all complete
output hashes matched. The input's full hash was unchanged afterward.

| Operation | Column | Output bytes | Worker seconds | Peak driver RSS bytes | Output SHA-256 |
| --- | --- | ---: | ---: | ---: | --- |
| Trim | 1 | 18,235,785,240 | 35.552 | 4,440,064 | `a045c2f7d846ae43d813c7f61402ce74b599f5142c46adffe9c9c5b4409d02f0` |
| Uppercase | 2 | 18,253,611,032 | 38.015 | 4,456,448 | `7b339f6243da2535d33f2c93dd2169099459e7352b62d7a04c7863a07ff38600` |
| Lowercase | 2 | 18,253,611,032 | 35.935 | 4,456,448 | `ae8187bf4a4c7b27504043cf5b42f2dbb9be274dfad09dcab50d03309cbded9a` |
| Fill empty with `filled` | 3 | 18,280,349,720 | 40.497 | 4,440,064 | `314b0763639cbf20453b62b4a62a583e06a19e645300f5b05e0a6d4ddf323535` |

Worker time includes the worker's counting/storage preparation and output
materialization through the public session/job API. It excludes fixture
generation and independent hash verification. RSS is macOS `/usr/bin/time -l`
maximum resident set size for the benchmark process, not the desktop app.

A separate uppercase job was cancelled after 72,347,672 observed written bytes.
It reported cancellation after 6.176 seconds, left no output or staging file,
and preserved the source.

## Connected native checks

The packaged egui app was operated through VMPal.

- Selected numbered columns, opened Clean Up Columns, and checked labeled
  sample values and all four visible operation choices.
- Entered a pending cell edit before Trim; preview and Apply both used it.
- Trim, uppercase, lowercase and fill reported the exact expected counts.
  Unicode checks included `Straße` and context-sensitive Greek `ΟΣ → ος`.
- Undo and Redo restored the previous/cleaned data.
- Fill changed the empty field and preserved the whitespace-only field.
- Save and Save As produced independently checked exact bytes. Save As retained
  the original source. The final combined output's SHA-256 was
  `b13cf7d85a6cd07ee25915135b9f1710d88fb90481fab855fd9c5dd976d0177f`.
- An active filter disabled cleanup and displayed
  “Clear the filter before editing the document.”
- Native Trim of the 17 GiB fixture completed with 4,456,448 changed cells.
  Its entire output matched the Trim hash above, and the source matched its
  original hash.
- A subsequent native uppercase job was cancelled after writing began.
  The previous Trim version remained intact and only its generation file
  remained in the private working directory.
- Discard restored the source and removed that private working directory.
- Sampled native application RSS peaked at 141,574,144 bytes (about 135 MiB),
  sampled once per second across cleanup and subsequent checks. This is a
  sampled peak, not an OS-recorded maximum. The app remained responsive to
  selection, dialogs, progress, Cancel and Discard.

## Automated coverage and checks

The workspace suite covers core exact outputs and counts, Unicode whitespace
and full case mapping, quoted/empty/missing/multiline fields, invalid selected
UTF-8, preserved non-target bytes, pending edits, output bounds, source changes,
small read chunks and BOMs, no-op/cancel behavior, and structural history.

Focused egui checks also cover operation/fill accessibility labels, filter
gating, preview bounds, effective pending values, and cancellation at the
completed-worker handoff. The source-change warning regressions failed before the fix and passed after it
for both job startup and publication handoff.
The cancellation handoff regression passes without
a production fix: the shared worker's publication protocol already protects it.

The final workspace run passed 395 tests; strict Clippy and formatting passed.
The packaging self-test passed its rollback and license checks, including 17
license-generator tests.

Validation commands:

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
./scripts/macos-app.sh package
./scripts/macos-app.sh self-test
```

## Reproduction

Generate a synthetic fixture in an expendable directory with enough disk space:

```python
from pathlib import Path
header = b"name,word,empty,padding\n"
prefix = "  Alice  ,Straße,,".encode()
row = prefix + b"x" * (4096 - len(prefix) - 1) + b"\n"
rows = (17 * 1024**3) // len(row)
with Path("large.csv").open("wb") as output:
    output.write(header)
    block = row * 4096
    for _ in range(rows // 4096):
        output.write(block)
    output.write(row * (rows % 4096))
```

Run the public core job driver, one operation at a time:

```sh
cargo build --release -p quarry-core --example cleanup_benchmark --locked
/usr/bin/time -l target/release/examples/cleanup_benchmark large.csv trim.csv trim 1
/usr/bin/time -l target/release/examples/cleanup_benchmark large.csv upper.csv upper 2
/usr/bin/time -l target/release/examples/cleanup_benchmark large.csv lower.csv lower 2
/usr/bin/time -l target/release/examples/cleanup_benchmark large.csv fill.csv fill 3
target/release/examples/cleanup_benchmark large.csv cancelled.csv upper 2 67108864
```

Remove complete synthetic outputs between runs to bound disk consumption.
The driver is an example for verification, not a new product CLI command.

## Limits of this evidence

The benchmark uses a repeated synthetic dataset and local VM storage, with
warm filesystem caches possible. It does not predict every real dataset,
removable/network storage performance, or another operating system. Only Trim
completed at full size through the connected native GUI; all four operations
completed at full size through the shared core job API. No binary release or
host-Mac app validation is represented here.
