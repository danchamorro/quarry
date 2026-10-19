# Integrated file-joining acceptance evidence

This report records the cumulative implementation and validation for
[#71](https://github.com/danchamorro/quarry/issues/71) and #119–#128.
The implementation is in [PR #137](https://github.com/danchamorro/quarry/pull/137).
It does not claim that review, merge or the installed VM update has completed.

[Final review fixes and rebuilt-engine measurements](2026-10-19-join-review-followup.md)
record the later regression checks, provenance checks and complete release rerun.

All development and application work ran in VMPal macOS VM 1, macOS 27.0.1
(26A434), Apple M3 Max (Virtual), eight virtual CPUs and 16 GiB RAM, using
Rust 1.88.0. Every file, edit and foreign sentinel was synthetic. The host
application was not changed.

## Source and checks

- Engine and complete working-input native workflow: ac6c4f55dba1d0b96348646381b651ea2785591e.
- Keyboard visibility follow-up: 614ac9e253741bc0bace8456d0009b6d21756ae5.
  This changes the dialog's focus scrolling and its tests, not core joining,
  comparison, serialization or resource calculations.
- The cumulative workspace passes 674 tests: 361 core and 266 egui tests,
  plus the existing other workspace suites. Formatting, all-target/all-feature
  Clippy with warnings denied, diff checks and release packaging pass.
- All 17 packaging self-tests passed before the focus-only follow-up; packaging
  and signature verification passed again afterward.
- The ac6c4f5 macOS, Linux core/CLI, Windows core and GitGuardian checks passed.
  The native Windows job ran all fourteen public-job fault tests and the final
  private-owner completion/cancellation/drop check. The final submitted head
  still requires its own green CI and actual CodeRabbit review.

Current test evidence is in the VM logs beginning
/tmp/quarry-128-focus- and /tmp/quarry-128-samples-. Earlier reports below retain
their original revisions, narrower scopes and historical remaining-work notes.
Their test paths run again in the cumulative suite.

## Release measurements

The committed [release probe](../../crates/quarry-core/examples/join_acceptance.rs)
uses real public analysis and creation jobs, without fault instrumentation.
The [fixture and verification harness](../../scripts/bench-join-acceptance.py)
generates expected complete-output hashes independently of Quarry.
[Machine-readable results](2026-10-19-join-acceptance.json) retain exact counts,
bytes, source and executable hashes, input/output hashes, timings and resource
measurements.

| Case | Input bytes | Result rows / bytes | Engine seconds | Peak RSS bytes | Tracked peak temporary bytes |
|---|---:|---:|---:|---:|---:|
| wide complete | 54444628 | 32 / 32921700 | 0.617952 | 49807360 | 117741812 |
| skew complete | 663064 | 1572864 / 814743616 | 6.141567 | 5144576 | 1756066368 |
| large complete | 19330006916 | 73728 / 9674630442 | 167.251143 | 24068096 | 38692391148 |
| large cancel | 19330006916 | Cancelled; no result | 91.182749 | 23969792 | 28931930841 |

The large case writes two real, non-sparse 73,728-row inputs with 128 KiB
payloads, totaling over 18 GiB in the 16 GiB VM. Both their logical bytes and
guest allocated bytes exceed RAM. Only 74 keys match; the exact 73,728-row
result is about 9 GiB and preserves original Main order rather than key order.
The full-result SHA-256, byte count and input hashes pass.

The skew case expands one 1,024-by-1,536 key group into 1,572,864 pairs.
A 128 KiB group target and 1 MiB run target force bounded blocks and external
passes. The measured group buffer is 130,950 bytes, below 131,072. The complete
814,743,616-byte result matches the independent nested-loop streaming oracle.

The wide case joins 32,768 columns per input into all 65,536 output columns,
with sixteen key pairs and independent tab/comma delimiters. Its 32-row result,
including every heading and value, matches the full-result oracle.

The second large run cancels a real creation job after its row-progress threshold
of 1,000. Cancellation plus cleanup completes in 0.151777 seconds after the
request, leaves no published result or owned unpublished data, and retains both
input hashes. Analysis is included in the total job time above but precedes that
cancellation interval.

RSS is macOS /usr/bin/time -l's process maximum, not a proof of an absolute
process-memory cap. The engine's 512 MiB tracked-payload ceiling remains distinct
from allocator/runtime and existing editor state. Temporary-byte counters record
the exact tracked logical file-byte peak. Independent du -sk samples every
0.5 seconds measure allocated bytes for the complete fixture directory, including
inputs and results; those samples can miss shorter peaks. The cancellation
directory retains the earlier completed result. Detailed allocated-byte peaks
are in the JSON, rather than conflated with temporary-only counters.

Each timed case ran without a concurrent build/test workload. These are synthetic
acceptance measurements, not controlled performance comparisons or guarantees for
network drives, every filesystem or every possible record shape.

Reproduce inside the configured macOS VM, using new case directories whose
parent already exists:
```sh
cargo build -p quarry-core --release --locked --example join_acceptance
python3 scripts/bench-join-acceptance.py --case wide --root /path/to/new/wide
python3 scripts/bench-join-acceptance.py --case skew --root /path/to/new/skew
python3 scripts/bench-join-acceptance.py --case large --root /path/to/new/large
python3 scripts/bench-join-acceptance.py --case large --root /path/to/new/large --reuse --cancel-after-rows 1000
```

## Native integrated workflows

Build 203, clean source ac6c4f5, executable SHA-256
dc998751cdb5194d8f5ac246725a8b10e6103634f67b6d6ed66922824a0df891:

1. Opened Main and edited Bob to Current Bob without saving.
2. Selected that current document and a saved Additional input with native
   controls, chose Full mode, and paired account plus region.
3. Analysis showed 5/6 input rows, 3/4 matching rows, 7 matching pairs,
   2/2 unmatched rows, 1/1 empty-key rows, one duplicate group per side,
   and an eleven-row result. Separate unmatched and duplicate samples identified
   their comparison tuples and data-row numbers.
4. Selected a new output. Clicking Create before acknowledgement produced no
   file; acknowledging the 2-by-3 expansion then created exactly eleven rows
   and 366 bytes. All bytes matched the independent oracle.
5. Opened the result in an ordinary tab, used Control-Tab to confirm Main still
   held its unsaved value, edited the result and used Command-Z to restore it.
6. Keep Editing cancelled quit and retained Main's value. A subsequent explicit
   discard of only synthetic edits closed the session. Original input and saved
   result hashes remained unchanged.

Build 204, clean source 614ac9e, executable SHA-256
56c527f97282a3ac0d75fc889c18fb0ff59d7dfbc8f3c4aca977e1ee00b6a658:

- Ordinary native Tab navigation reached the comparison checkbox; Space toggled
  it. Further Tab navigation scrolled the previously clipped Add key pair button
  into view while footer actions stayed visible. Space added the region pair.
- The default Left workflow then created nine rows and 315 bytes, matching an
  independent complete-output oracle. Unmatched Additional examples remained
  visible even though those rows were omitted from this result.
- The result opened in an ordinary nine-row/six-column tab and the clean
  synthetic session quit normally.

Both results were 0600 files without inherited ACL entries. Original sources
retained their hashes and no output staging remained. All candidate bundles
stayed local; neither candidate was installed over the VM baseline.

## Requirement-to-evidence audit

| #71 requirement | Current evidence |
|---|---|
| Current/saved inputs, aliases, valid drafts, idle/unfiltered all-row capture, self-join rejection | Current-document picker/alias/draft/filter regressions, both-working-input oracle, build 203 native current Main; [working-input evidence](2026-10-19-current-document-joining.md) |
| Independent formats, header-only and no-schema empty inputs | 100 mixed-format/header cases and malformed excluded-field tests; [native UTF-16 BE plus Windows-1252/ragged flow](2026-10-19-join-input-formats.md) |
| One to sixteen exact key pairs, explicit ASCII case, significant zeros/whitespace/Unicode, empty components | Independent all-mode/two-case tuple oracle, tuple framing/NUL/bound tests, actual sixteen-key wide result; [native pair/case controls](2026-10-18-composite-key-joining.md) |
| Four row-retention modes and absent-side empty fields | Complete all-mode oracle with no matches, duplicates, header-only and zero-row cases; [native four-mode comparison](2026-10-18-join-modes.md), integrated native Full and Left |
| Every duplicate pair, uniqueness validation, exact acknowledged expansion | Skew full-result hash and bounded group measurement; public uniqueness/overflow/cancel cases, native blocked-before-ack and 2-by-3 expansion |
| Stable mode-specific Main/Additional order | Nested-loop byte oracles, ordinal validation, independent larger-than-RAM and skew hashes, native unsorted-key source order |
| Select/reorder/rename all discovered output columns; late review and missing-field padding | 32 projection/header combinations, late-column/layout-recovery tests; [native projection](2026-10-18-join-output-columns.md) and mixed-format late-column review |
| Deterministic names, duplicate-name warning, configurable header | Header naming/limits and duplicate-warning tests, headerless reopen; native disambiguated role headings and earlier headerless projection |
| Full exact analysis with bounded result/unmatched/duplicate examples, limits and strict diagnostics | Exact oracle count/byte assertions; bounded per-side example test in every mode; native new counts/samples; malformed/overflow checks |
| UTF-8/no BOM, Main delimiter, LF, exact fields/newlines, 65,536 columns and record/header limits | Mixed-format and projection byte/reopen matrices, BOM-looking first field checks, serialized-size tests and real 65,536-column case |
| Input/private-generation leases through analysis/preview/write, release and invalidation | Current structural/header/cell overlays in both inputs, alias/Save/Discard/reload/close guards, failure/reset release tests; native retained Main history |
| Off-thread phases, two-operation limit, no idle-preview slot, bounded capture/preview | Real public jobs and fault checkpoints, operation-limit/idle-preview tests, O(1) shared capture, group/preview bounds, responsive native workflow |
| Source/revision/alias/live-output/storage rechecks, owner-only ACL storage, no-clobber publication and actual-plan validation | Real pinned-directory/source/output-race and inherited-ACL regressions, current/history-root exclusion, exact output counts/bytes; [recovery matrix](2026-10-19-join-recovery.md) |
| Pre-publication cancel/failure cleanup, foreign-data preservation and reported cleanup failure; post-publication completion | Fourteen real-public-job fault tests covering 116 targeted scenarios, actual partial staging bytes, source/destination/foreign-file checks, native final-close failure evidence |
| Actual rows/bytes, ordinary editable result tab, unchanged source histories, tab limit and safe close/quit | Full tab-strip and cross-document harness, background retirement and spawn/cleanup-failure regressions, integrated native edit/Undo/Control-Tab/Keep Editing/quit |

The keyboard audit found that Tab could focus a clipped control without revealing
it. The full-app raw-key regression failed even after allowing repaint frames;
it now verifies visible focus, Space activation, and manual scrolling without
snap-back. The dialog reveals newly focused clipped controls in its own content
layer and does not follow unrelated-document, footer or popup focus. Native
build 204 independently confirmed the fix.

## Observations and remaining delivery gates

One transient raster artifact appeared after shrinking the VM window and opening
quit review in build 203 (VMPal frame 1691). Restoring the size repainted it.
Controlled repeats of the same two-tab background-unsaved-document path rendered
normally in both the unchanged installed build 157 and the same build 203.
Its cause is unproven; this report does not call it a reproduced fixed defect.
No source or result bytes changed, and unsaved-work review remained intact.

Disk-full errors in the recovery matrix are injected OS errors at real I/O
boundaries; a physically full volume was not mounted. Platform-specific core CI
does not establish a Windows or Linux desktop port.

Before #71/#128 can be closed, the submitted final head must receive an actual
completed CodeRabbit review and required green CI, findings must be resolved and
reviewed, and the integrated change must merge. Then update the clean VM main
checkout, safely install with the rollback-preserving installer, verify the
installed merged revision and clean SourceStatus, reopen it and verify the
feature. Merge and installation completion are recorded separately on #71.
At this report's preparation the installed VM app was still build 157, revision
21805ecff291605f45789e49e67aa564722e5f3d.
