# Inspect File and recovery validation

Acceptance evidence for #62 and #109–#116. Development, builds and native checks
ran in the VMPal macOS VM (macOS 27.0.1, Apple silicon, 16 GiB RAM), using synthetic
files.

## Automated coverage

- All five encodings: logical-record/native-byte positions, BOM handling,
  quoted multiline data, escaped quotes and empty fields.
- Broken quoting and incomplete UTF-16 code units stop at uncertain boundaries.
  Aligned invalid UTF-16 characters retain complete record ranges for recovery,
  while ordinary opening remains strict. Invalid UTF-8 stays read-only;
  complete affected records can be explicitly omitted.
- Bounded record buffers, field-count limits and 1,000 retained findings.
  Total finding counts and coverage continue beyond the display cap.
- Unsaved header/cell values are inspected without invented source offsets.
  Edits, source replacement and format changes invalidate results.
- Failed opens preserve the current document and retain the failed target.
  Inspect Again and Details keep that target. Failed format retries retain the
  old session and report.
- Recovery deduplicates selected records, protects headers, preserves native
  retained bytes and edits, validates staging, rejects existing destinations
  and stale sources, and cancels without publication.
- Late operation errors retain known data row/column or logical-record context
  in desktop and CLI details. Unknown original-file positions are explicit.
- AccessKit activates the omission checkbox, and the focused Close button
  responds to Space. Controls and source previews expose accessible text.

Checks:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
./scripts/macos-app.sh package
```

## Native desktop checks

- Opened an uneven CSV and a separate unterminated-quote file. The failed target
  stayed in the issue window while the valid tab remained open. Inspect Again
  reported stopped coverage and disabled unsafe omission.
- Inspected the uneven CSV, navigated to the affected record, selected it,
  reviewed the removal count and destination, and created a recovery copy.
  Output matched the expected CRLF bytes; the source was unchanged.
- Inspected a Windows-1252 fixture in default UTF-8 mode, explicitly changed
  its encoding and retried. Its accented value displayed correctly and the
  new inspection had no encoding findings.
- Inspected unsaved cell edits, then edited another cell with the report visible.
  The report became stale and recovery was disabled; unsaved values remained.
- Multiple synthetic tabs retained separate reports and documents.
- Native inspection of the 4.06 GB file displayed live progress; Cancel operation
  returned a partial report after 2,247,098,368 bytes and 18,571,061 records.
  Recovery stayed disabled and the already-open grid remained usable.

## Representative performance

Release CLI measurements use warm filesystem caches and repeated synthetic rows.
They are VM measurements, not throughput guarantees or a beyond-RAM benchmark.
RSS is the CLI process maximum reported by macOS time.

| Input | Bytes | Records including header | Full inspection | Peak RSS |
|---|---:|---:|---:|---:|
| UTF-8 | 62,914,585 | 524,289 | 0.261 s | 3,850,240 B |
| UTF-16 LE | 125,829,172 | 524,289 | 0.502 s | 5,406,720 B |
| UTF-8 | 4,060,086,292 | 33,554,433 | 11.936 s | 3,850,240 B |

Cancellation after the first progress update returned a cancelled, partial
report in 18–20 ms wall time. The 4.06 GB run stopped after 3,145,728 bytes without
implying the remaining data was clean.

Recovery of the 62.9 MB fixture, including inspection, writing and validation,
took 0.677 s. Output matched retained source bytes and the source hash was unchanged.

Three warm ordinary-open runs on the 4.06 GB file, using open --no-wait
--metrics-only, had median first-100-row times of 1.683 ms on baseline 7b5285a and
1.654 ms with this feature. Both read 1 MiB of bootstrap data. Peak CLI RSS was
4,587,520 B versus 4,669,440 B. The first baseline run was a 24.360 ms outlier;
these few runs establish bounded opening, not a statistically meaningful speedup.

Generated CSVs, logs and application bundles stay out of Git. Existing user
documents and the host Mac application were not used for validation.
