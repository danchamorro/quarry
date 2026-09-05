# Numeric sorting validation: 2026-09-04

## Result

The Number mode sorted a deterministic 1,000,000,148-byte CSV with 5,408,918
data rows in 6.971 seconds. Peak process RSS through sorting was 28.72 MiB;
peak temporary storage was 2,209,665,865 bytes, below the 4,411,078,360-byte
allowance. The worker created 76 runs and completed two merge passes.

A complete independent Python Decimal scan verified numeric order, increasing
source row IDs within equal numeric keys, original amounts and multiline
payloads, and the exact row count. The source SHA-256 remained unchanged.
The CLI verified raw-header preservation; the worker verified its bounded dual
record fingerprint. Equivalent numbers retained their original text and order.

The validation covers this 1 GB workload. Existing 12 GB and 50 GB text-sort
results remain evidence for the shared external-merge architecture; this report
does not claim new numeric measurements at those sizes.

## Environment and workload

- Apple M3 Max, 16 cores, 128 GiB RAM.
- macOS 26.6.2 (25G83), Rust 1.88.0, locked release builds.
- Baseline: main commit `7a8fd9ca2a2679867620d0cc5e16835648676809`.
- Candidate: `codex/numeric-sorting`, the accompanying changes.
- Warm filesystem caches, not purged; timings are not cold-cache claims.
- Three columns: amount, original row ID, and a 166-character payload, with
  embedded newlines every 211 rows. Amounts range from -1000 to 1000 and mix
  signs, leading/trailing zeros, scientific notation, surrounding whitespace,
  repeated equivalent values, and blanks every 997 rows.
- Source SHA-256:
  `fb160ff92e2f8efcc759fd60dd28e786820613068be312f2834a8449e6b1366d`.

## Text compatibility and timings

Three alternating baseline/candidate runs checked the existing ascending,
case-sensitive Text path on the same source. Every output had fingerprint
`9718022defe921f7`; the first pair was also compared byte for byte.

| Mode | Worker seconds | Median seconds | Peak RSS |
|---|---|---:|---|
| Baseline Text | 6.061, 6.477, 6.506 | 6.477 | 29.28–29.33 MiB |
| Candidate Text | 7.031, 6.478, 6.413 | 6.478 | 29.36–30.33 MiB |
| Candidate Number | 6.971 | 6.971 | 28.72 MiB |

These are local samples, not a broad performance guarantee. Worker timings
exclude the CLI's later validation reads and the independent Python scan.
Number output fingerprint: `81f7866adc6bb12d`.

## Cancellation, regressions, and desktop

Cancellation requested at 64 MiB stopped the Number worker after 65 MiB,
with 2.329 ms cancellation latency and 28.31 MiB peak RSS. No destination or
unpublished sort run remained.

Focused regressions cover both directions, precision beyond 2^53, long decimal
values, signed zero, scientific notation, equivalent-value stability, blanks
and ragged rows, sparse edits, forced multipass merging, key-size limits, and
invalid-input cleanup. A separate temporary cross-check compared the actual
Rust key implementation against Python Decimal for 20,018 adversarial values
and rejected 21 malformed values. No mismatch was found.

The desktop regression selects Number through accessibility actions, switches
back to Text and retains the case preference, sorts, undoes/redoes, and saves
an exact copy while preserving the source. Native package smoke testing checks
the dialog layout, actual numeric order, Undo, and rejection of a text column.
The Number failure identifies the data row and column and leaves the document
unchanged. The package is a local development candidate, not a public release.

## Reproduction

Create an absent `numbers.csv` with Python 3:

```python
from pathlib import Path

written = 0
row = 0
with Path('numbers.csv').open('xb', buffering=8 * 1024 * 1024) as f:
    header = b'amount,row_id,note\n'
    f.write(header)
    written += len(header)
    while written < 1_000_000_000:
        cents = ((row * 104729) % 200001) - 100000
        sign = '-' if cents < 0 else '+'
        whole, fraction = divmod(abs(cents), 100)
        amount = [f'{sign}{whole}.{fraction:02}', f'{sign}{whole:06}.{fraction:02}',
                  f'{cents}e-2', f' {sign}{whole}.{fraction:02}00 '][row % 4]
        if row % 997 == 0:
            amount = ''
        note = 'x' * 166
        if row % 211 == 0:
            note = '"' + note + '\nline two"'
        record = f'{amount},{row},{note}\n'.encode()
        f.write(record)
        written += len(record)
        row += 1
```

Build and run, using a new output path each time:

```bash
cargo build --release --locked -p quarry-cli --bin quarry-bench
target/release/quarry-bench sort-save-as numbers.csv sorted.csv \
  --column 1 --mode number --order asc --header first-row --cache-state warm
```

Use `--mode text` for compatibility measurements (omit that option on the
baseline binary), and add `--cancel-after-bytes 67108864` for cancellation.
The CLI reports worker metrics and verifies complete output order and header
preservation. For independent validation, read the output with `csv.reader`,
compare stripped nonblank amounts with `decimal.Decimal`, and assert increasing
row IDs for equal keys. Regenerate each expected amount and payload from that
row ID using the generator above, then verify the source SHA-256.
