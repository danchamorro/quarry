# Join result-column validation

Date: 2026-10-18. [#124](https://github.com/danchamorro/quarry/issues/124), part of [#71](https://github.com/danchamorro/quarry/issues/71).

## Automated checks

All 592 workspace tests passed in the VMPal macOS VM (macOS 27.0.1, build 26A434), together with formatting, all-target/all-feature Clippy and the release build.

The new core matrix compares an independent nested-loop result with 32 output combinations: all four modes, header enabled/disabled, and four projections. It verifies exact fields, order, serialized bytes, reopen behavior, output from only one side, omitted keys, padding, embedded quotes/newlines and a BOM-looking value placed first.

Other checks cover deterministic default headings, explicit duplicate warnings, source-column bounds, selected-column limits, serialized-header/record limits, and a valid zero-row header-free result. The full-input validation and guarded publication regressions still run when fields are excluded.

The UI harness activates real controls, including keyboard focus plus Space: include/exclude keys, per-source select-all/none, move Additional's field first, type an output heading, disable the header, analyze/create/open, preserve the first row as data, warn about duplicate names, and reset layout after replacing an input.

The larger metadata reserve made the old synthetic 2 MiB / 4,096-column resource configuration insufficient. That test now uses a 4 MiB budget while retaining its 2,048 repeated-heading assertions. Names and collision bookkeeping are additionally bounded against remaining memory.

## Native VMPal check

The release binary was run directly inside the VM; no host application or installed VM bundle was changed. Binary SHA-256:

    182bbf0c21133894c27891459e2104aeae409ad6ecb11e957d7fdbe95122b3e5

Synthetic inputs:

- Main: id,name, followed by b,Bob, a,Alice, z,Last.
- Additional: id,city, followed by a,Seattle, b,LA.

Through native file pickers and the visible Join Files dialog:

1. Selected both files and retained the default key pairing.
2. Excluded both key columns.
3. Moved Additional column 2 (city) before Main column 2 (name).
4. Set its output heading to Destination.
5. Disabled the output header and analyzed the complete inputs.
6. Created joined.csv through the native save picker.
7. Opened the saved result as an ordinary document. The grid showed three rows and two columns, beginning with LA / Bob and using generated Column 1 / Column 2 headings.

Exact saved bytes: LA,Bob\nSeattle,Alice\n,Last\n (27 bytes).
SHA-256:

    0b5213da0dcf4752cdfaa95371b7abfeddaab7ce01bf7eab39f02a90714fd3c9

Both source files retained their exact bytes. The synthetic session then closed normally.

The first native pass exposed a nested scroll-list sizing problem: the fourth column was clipped while the heading editor appeared below it. Reserving the list height and deriving row height from its controls fixed the native reproduction; the repeat showed all four choices together. A component-only check did not reproduce this nested layout and was not retained or claimed as a regression test.

## Existing-output review follow-up

A subsequent review exposed that choosing an existing destination consumed the retained analysis. The added UI regression first failed because the plan disappeared; it now verifies that existing files and dangling links are rejected before starting a writer and that a fresh destination can be written using the same preview. This fix is inherited from #120 and was rechecked through the stacked branches. The native happy-path binary recorded above predates this preflight-only follow-up.

## Remaining parent work

This slice still uses the saved UTF-8/headered inputs provided by the preceding tickets. Mixed source formats and late ragged-width discovery belong to #125; current unsaved documents and lifetime leases to #126. The final fault/cancellation matrix and larger-than-RAM release evidence remain #127 and #128. This evidence does not close #71.
