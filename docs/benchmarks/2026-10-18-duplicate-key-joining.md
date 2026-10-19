# Duplicate-key join validation

Date: 2026-10-18. Duplicate-key slice of [#71](https://github.com/danchamorro/quarry/issues/71), tracked in [#121](https://github.com/danchamorro/quarry/issues/121).

## Release probe

VMPal macOS VM, Apple M3 Max (Virtual), 16 GiB RAM, macOS 27.0.1 (26A434), Rust 1.88.0. Source revision: 09eeb7451c5188ec0e3110a101c938dfcfbc1fa0. The release example used the default 8 MiB duplicate-row block budget.

Both inputs have one repeated key. Main has 30,000 rows and 11,970,008 bytes; Additional has 80 rows and 569 bytes. Each Main value consists of its six-character ordinal label and 390 repeated ASCII characters. Additional values carry their ordinal. The expected Cartesian product contains 2,400,000 rows and 974,400,037 bytes.

| Measurement | Value |
| --- | ---: |
| Analysis seconds | 0.167890 |
| Total example seconds | 5.498859 |
| Maximum RSS bytes | 43,843,584 |
| Tracked peak temporary bytes | 2,153,552,677 |
| Tracked peak retained duplicate-row block bytes | 8,388,553 |
| Configured duplicate-row block bytes | 8,388,608 |

The independently generated output SHA-256 is:

9d70bcbbc4e2bfa6a8b2c422f5782af57c72d419ac549580ae7522a561d7a1a5

A Python streaming oracle enumerated every Main/Additional ordinal pair without using Quarry's parser or join code. The complete output hash and byte count matched. Both input hashes were unchanged and no owned spill directories remained. The retained block stayed below its configured limit even though the Main duplicate group exceeded that limit.

The analysis projected 2,400,000 matching pairs and result rows, one duplicate group per input, 3,115,200,037 additional temporary bytes, and 12,752,640 retained frame bytes. The measured tracked peak was below the combined allowance. RSS comes from /usr/bin/time -l; temporary and block peaks are engine counters, not independently sampled filesystem allocation or a proof of a hard process-RSS limit. This single warm run followed builds and tests; it is a correctness/resource probe, not a controlled performance comparison.

## Native desktop

Candidate build 165, revision 09eeb7451c5188ec0e3110a101c938dfcfbc1fa0, clean source, was packaged and operated in the same VM. The installed main app was not replaced by this candidate.

- Selected a three-row Main file and two-row Additional file using native file pickers.
- Requiring a unique Main key rejected the repeated key and identified the source record.
- Turning off that requirement produced the exact five-row preview: four matching pairs and one unmatched Main row, one duplicate group per input.
- Chose a new output path. Create Joined File stayed disabled before acknowledging the displayed five-row result; clicking it created no file.
- Acknowledged the expansion, created the result, and verified all 90 output bytes against a handwritten oracle. Both source files were unchanged.
- Opened the result as an ordinary five-row document, then quit normally.

## Regression coverage and limits

All 569 workspace tests, formatting, all-target/all-feature Clippy and release build passed. The 19 core join tests include forced small spills, an independently checked 19,200-row Cartesian result, a group larger than its 512-byte block budget, cancellation during actual pair generation, optional uniqueness per input, one-to-many creation without acknowledgement, overflow/record-size rejection and the saved-file slice's source/output/privacy regressions. Eight dialog tests cover acknowledgement and reset, uniqueness rejection, reservations, operation limits, tab limits, close cleanup and cleanup-failure quit recovery.

A separate analysis test projects 25 million pairs while retaining less than 2 MB of input frames; it does not materialize that output during analysis. The release probe here remains smaller than RAM. Other join modes, composite keys, mixed formats, unsaved inputs and the complete larger-than-RAM acceptance matrix remain assigned to the later #71 tickets.
