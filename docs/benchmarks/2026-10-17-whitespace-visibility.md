# Whitespace visibility validation

Issue: [#102](https://github.com/danchamorro/quarry/issues/102).

## Environment and method

Validated in the configured VMPal macOS VM on 2026-10-17: Apple M3 Max (Virtual),
16 GiB RAM, macOS 27.0.1 (26A434), arm64, Rust 1.88.0. Native checks used release
builds in locally signed app bundles. The baseline was installed build 149,
revision 087793fca24d142d8ea7f5498c94e6b62a288663. The candidate included the
whitespace changes; no parser, index, or export algorithm changed.

The synthetic large CSV was 18,270,331,643 bytes (17.02 GiB), exceeding guest
physical RAM, with eight columns and 268,681,347 data rows. Repeated records
contained edge spaces, normal word spaces, repeated spaces, a quoted tab,
a non-breaking space, an empty field, and trailing whitespace. The fixture was
written as complete 68-byte records in approximately 16 MiB blocks.

For each native process, ps RSS/CPU was sampled about every 100 ms for
20 seconds while the large document was open and indexing/navigation were
available. The filesystem cache was uncontrolled. The baseline sample began
after its first visible frame; the candidate sample began shortly after launch.
A candidate build ran concurrently with baseline observation, so these are
memory observations, not a comparable CPU or startup-latency benchmark.

| Release build | View | Maximum RSS in sampled interval |
| --- | --- | ---: |
| Baseline build 149 | Existing grid | 125.63 MiB |
| Candidate | Show All Whitespace enabled | 126.09 MiB |

These samples demonstrate bounded memory on this dataset; they do not establish
an overall process peak or a latency improvement. Both builds accepted page
navigation. The candidate completed indexing, accepted a jump to row
200,000,000, and displayed rows 200,000,000–200,000,035 of 268,681,347 with the
expected indicators. The preference survived an application restart.

## Native correctness checks

- In the production monospace font, edge/repeated spaces display as muted dots;
  tabs, CR, LF, and NBSP have distinct readable labels. The triangle survives
  clipped text and selected-cell backgrounds.
- Normal New York is unmarked by default and displays New·York with Show All
  Whitespace. Clipboard output remains exactly New York.
- A 20,000-character field with a space and tab beyond the preview reports two
  trailing whitespace characters. The same check passed with a 32 MiB field,
  whose full-value inspection runs on the background worker.
- A 65-column native grid retained the final-column edge marks after horizontal
  navigation. Filtering to the exact space-padded value returned one match and
  preserved its indicator and full-value tooltip.
- Native editing followed by Save As preserved every unedited field, including
  tabs, embedded CRLF, NBSP, repeated spaces, and edge spaces. An independent
  CSV reader confirmed the expected values. The existing serializer removed
  optional quotes around a tab-containing field; view marks were not written.
  The source fixture stayed unchanged.

## Automated validation and bounds

Workspace tests: 412 passing tests, including 17 added whitespace checks.
Rustfmt, workspace Clippy with warnings denied, and the release workspace build
passed.

Coverage includes Unicode whitespace and invalid UTF-8, expanded-mark preview
limits, hidden tails, cancellation, lazy snapshots, queue/cache limits,
preference persistence/failure, keyboard/accessibility operation, generated
headers, empty/missing fields, filtered/wide grids, physical column identity,
separate documents, header/cell edits, Undo/Redo, discard, cleanup replacement,
raw copy, and Save As.

Each document keeps at most 16,384 summaries with bounded LRU bookkeeping.
There is one worker per document and at most 128 requested cells per batch,
normally limited to 8 MiB of selected field bytes. A single larger field may
exceed that batch limit, subject to Quarry's existing 64 MiB record limit.
Loaded source rows are shared with the worker; edited/header snapshots are
copied only on a cache miss. Cancellation is checked at 16 KiB intervals.
Metadata does not retain complete field values after analysis.

Rendering retains the existing 16,384-character cell preview bound (120 for
headers), including mark expansion. Full-value inspection covers a loaded cell
even when its whitespace lies outside that prefix. It does not provide a
file-wide whitespace count, and an unvisited cell is inspected when rendered.
