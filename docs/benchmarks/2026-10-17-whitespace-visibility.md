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
| Initial candidate | Show All Whitespace enabled | 126.09 MiB |
| Revised candidate with shared snapshots | Show All Whitespace enabled | 127.67 MiB |

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
- Following review, a 32 MiB header was opened with explicit Header row format.
  Its trailing-space indicator and tooltip stayed correct after repeated page
  navigation. Completed header summaries survive row-buffer changes.
- A 65-column native grid retained the final-column edge marks after horizontal
  navigation. Filtering to the exact space-padded value returned one match and
  preserved its indicator and full-value tooltip.
- Native editing followed by Save As preserved every unedited field, including
  tabs, embedded CRLF, NBSP, repeated spaces, and edge spaces. An independent
  CSV reader confirmed the expected values. The existing serializer removed
  optional quotes around a tab-containing field; view marks were not written.
  The source fixture stayed unchanged.

## Automated validation and bounds

Workspace tests: 418 passing tests, including 21 added whitespace checks plus borrowed storage-
estimate and Sort-dialog snapshot regressions.
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
Source sessions/rows and pending header/cell edits are shared immutably with
the worker. Rendering does not copy entire source headers or edited values;
core data jobs still receive owned snapshots when an operation starts. Cancellation is checked at 16 KiB intervals.
Metadata does not retain complete field values after analysis.

Rendering retains the existing 16,384-character cell preview bound (120 for
headers), including mark expansion. Full-value inspection covers a loaded cell
even when its whitespace lies outside that prefix. It does not provide a
file-wide whitespace count, and an unvisited cell is inspected when rendered.

## Review regression

The large-header navigation regression failed before the fix with
“scrolling must retain completed header inspection” and passed afterward.
A second check proves that source-header and pending-edit inspector snapshots
share the existing byte allocation and remain immutable after a later edit.
The existing owned storage-estimate API is retained as an adapter to the same
borrowed implementation; the exact-bound test and all existing storage tests
pass. Native Save As was repeated successfully after this representation change.

The zero-match-filter polling regression failed before its fix with
“zero-match polls must retain header inspection” and passed afterward. Pending
zero-match polls leave the shared inspector job and header summaries alone;
filter scan-error and row-buffer boundaries invalidate only row metadata.

The Sort estimate/dialog regression measured 4,194,304 bytes of owned pending-
edit snapshots across one direct estimate and three dialog frames before the
fix, and zero afterward. This is test-only accounting at the owned-job snapshot
boundary, not a claim of zero total UI allocation. The estimate still matches
the existing owned API's bound, uses the current editor draft, and preserves
pending values and source bytes. Header and cell payloads are borrowed; core
jobs continue to receive owned snapshots when they start.

The preference-path regression initially selected a working-directory-relative
path for an empty config root. Empty/relative roots now fall back to an absolute
user config directory, and absent/unusable roots do not create a relative
preference path. Tests cover empty, relative, missing, and absolute roots without
mutating the process environment.
