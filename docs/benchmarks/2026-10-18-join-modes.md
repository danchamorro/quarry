# Join-mode validation

Date: 2026-10-18. Row-retention modes for [#122](https://github.com/danchamorro/quarry/issues/122), part of [#71](https://github.com/danchamorro/quarry/issues/71).

## Automated checks

All 575 workspace tests, formatting, all-target/all-feature Clippy and release build passed in the VMPal macOS VM (macOS 27.0.1, build 26A434). The implementation includes the earlier header-cancellation review fix.

An independent nested-loop oracle covers all four modes across six data sets, including duplicate groups, empty keys, space-only keys, leading zeros, case differences, Unicode normalization differences, embedded quotes/newlines, unmatched rows, header-only inputs, no matches and asymmetric duplicates. Each case compares exact output bytes, preview order and counts, reopens the result, preserves both source byte sequences and checks owned spill cleanup. Tiny run and row-block budgets force external processing. A separate check rejects invalid ordinal keys and forbidden absent-side combinations.

The real egui workflow harness selects each mode through its rendered control and checks the written result order. Existing duplicate expansion, acknowledgement, cancellation, source/destination guards, private storage and document-lifecycle regressions remain green.

## Native desktop check

Candidate build 168, clean source revision bbafa40890fd5e35b9524bd4e32b54d5bf4da4da, was packaged and operated in the same VM. The installed main application was not changed.

Main contained x/A, z/Unmatched and x/B. Additional contained x/One, y/Extra and x/Two. Both were chosen through native file pickers, exercising the cancellable header worker.

| Mode | Preview rows | Verified ordering |
| --- | ---: | --- |
| Keep all Main rows | 5 | A/One, A/Two, unmatched z, B/One, B/Two |
| Matching rows only | 4 | A/One, A/Two, B/One, B/Two |
| Keep all Additional rows | 5 | A/One, B/One, unmatched y, A/Two, B/Two |
| Keep all rows from both files | 6 | Main-driven five rows, then unmatched y |

After acknowledging the six-row expansion, the full mode was saved through the native destination dialog. All 100 output bytes matched a handwritten oracle; both inputs were unchanged. Open Joined File displayed all six rows as an ordinary document, and the candidate quit normally.

All modes are written and reopened by the automated oracle; the native save/reopen above covers full mode. This is not the final #71 acceptance: composite keys, output layout, mixed formats/ragged width, working documents, fault/cancellation coverage and larger-than-RAM measurements remain tracked in the later tickets.
