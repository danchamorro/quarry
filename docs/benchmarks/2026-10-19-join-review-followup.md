# Final join review and release acceptance

This follow-up records the fixes for the completed full review of
[PR #137](https://github.com/danchamorro/quarry/pull/137) at c6cb68b, plus
Codex's pinned-capacity finding. The changes are 7512330ffa5a914a49d506a8e48562bcdea95d01
and 2d16932d4b960c9237a437f91ce55a8d8c77b6a7.
All source work, regression checks and release runs used the configured VMPal
macOS VM and synthetic data. The host application was not changed.

The [original acceptance matrix](2026-10-19-join-acceptance.md) maps every #71
criterion to cumulative automated and native evidence. Its ac6c4f5 measurements
remain historical. The complete measurements below use the rebuilt follow-up
engine and the provenance-enforcing harness.

## Review findings and failing-before checks

- **Pinned creation capacity:** the real public-job regression first failed
  with NotFound after the temporary directory was renamed. The fix reads Unix
  capacity and volume identity from captured handles and checks simultaneous
  spill/output allowances on a shared volume. Windows retains the conservative
  check against paths pinned by ancestor locks. Shared destination validation
  now resolves the captured output parent's current name. Six real creation
  cases rename either folder, leaving no replacement, a foreign directory, or
  a foreign file at the old name. All produce the exact Full result, preserve
  sources and foreign data, and remove owned staging. A capacity test also
  verifies the combined requirement and current diagnostic folder.
- **Combine Files cleanup reservations:** a gated real retirement test first
  accepted output inside a moved root awaiting cleanup. Metadata-only reservation
  snapshots now guard both choosing and starting output. Three cases cover
  moved/symlinked folders and a destination selected before retirement. They
  reject output, allow cleanup to finish, and preserve unrelated/source files.
  The snapshots retain no private-storage owner.
- **Rejected storage handoff:** after mechanical factoring, the real API's
  already-retained rejection left the first empty working state allocated.
  The shared helper used by all seven writer starts now cancels and drops/reaps
  the rejected worker before removing owned output and releasing empty state.
  The regression preserves source bytes, pending edits and Undo, and verifies
  no generation or private root remains. This is a defensive rejected-handoff
  check; ordinary fresh handoffs are not claimed to fail.
- **Publication plus cleanup errors:** existing wrappers first returned only
  Directory not empty, hiding a racing destination. They now preserve structured
  primary and cleanup errors in PublicationFailure carried by the I/O error,
  with the primary cause in its error-source chain and both explanations shown.
  Calls without cleanup failure retain their original typed error. Six real
  cases exercise both publish wrappers with destination races, source changes
  and cancellation. Existing and foreign files survive, partial output is
  removed, and both explanations remain available. Windows CI explicitly runs
  this portable regression.
- **Benchmark provenance:** the old harness accepted the ac6c4f5 executable but
  labelled its successful report with checkout revision 7512330 despite pending
  source edits. The harness now requires a clean checkout, builds the release
  example, obtains its exact Cargo artifact, verifies revision stability across
  the build, and checks revision plus binary hash before/after measurement.
  An optional executable copy must match that artifact. Dirty-source and
  stale-copy checks reject before creating fixtures. Three complete smoke
  cases correctly record the clean follow-up revision and binary hash.

The 679-test workspace suite passes: 364 core, 268 egui, and 47 tests in the
other existing suites. Formatting, all-target/all-feature Clippy with warnings
denied, diff checks, Python syntax and CI YAML parsing pass. The submitted final
head still needs its own platform CI and actual completed review before merge.

## Later uniqueness diagnostic correction

A subsequent review found that Require unique key reported the next logical
record when its selected input had no header. The public-job regression first
returned record 3 for the duplicate in record 2. The corrected checked offset
uses that input's own header flag. Eight cases cover both Main/Additional roles,
all four independent header combinations, and quoted multiline values. They
assert the diagnostic record and input path, unchanged sources, and complete
owned-analysis cleanup.

The cumulative workspace now passes 680 tests: 365 core, 268 egui and 47 other
tests. Formatting and strict Clippy pass. Windows CI explicitly runs this
portable diagnostic regression. This correction changes only an error record
number; it does not change successful join values, comparison, ordering or the
algorithm. The release measurements below retain their actual 2d16932 source
and executable hashes; the later diagnostic correction is not included in
that measured executable. The resulting submitted head requires fresh CI and
review before merging.

## Later Discard Changes feedback correction

The next completed full CodeRabbit review covered clean head 600c770. Its
Discard Changes finding was reproduced with the real analyzed-current-document
lease: header/cell edits and Undo history survived, but the action cleared the
notice instead of explaining the reservation. The regression failed on that
missing warning before the fix, then passed after an early action guard was
added. The menu now disables Discard Changes for a reserved document and
explains the reservation on hover. After the join closes, the same test verifies
that ordinary discard clears edits/history and leaves the original file intact.

The cumulative workspace passes 681 tests: 365 core, 269 egui and 47 other
tests. Formatting, strict Clippy and diff checks pass. This feedback correction
does not change the successful join engine or the measured binary below.

The review also suggested falling back to legacy Windows file IDs when
FileIdInfo is unavailable. That availability change was declined: Microsoft's
[BY_HANDLE_FILE_INFORMATION documentation](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/ns-fileapi-by_handle_file_information)
states that its 64-bit IDs are not guaranteed unique on ReFS. This shared stamp
also establishes private-file ownership for cleanup, so an unconditional
fallback would weaken that contract. Unsupported FileIdInfo backends continue
to return an error; no SMB compatibility claim or native Windows port is made.
Supporting those backends requires capability validation and direct backend
tests rather than treating a shorter ID as equivalent.

## Latest native candidate observations

Clean macOS VM candidate build 209 at 600c770 joined Main's current unsaved
Trim result with the saved Additional file using two keys. The preview reported
five/six input rows, seven matching pairs, nine Left result rows, two unmatched
rows per input and one many-to-many group producing six pairs. Create was
blocked before acknowledgement. The saved nine-row, six-column result matched
the independent 315-byte oracle exactly (SHA-256
cb68a5b7c14d4593d4f4b2a52fe9f0942d2cb28a6527b63976188b857a4b8cf6).
Both source hashes stayed unchanged; output was mode 0600 with no ACL entries
or unpublished staging. Opening, editing and Undo worked in an ordinary result
tab. A repeated complete join produced the same exact result.

One compact-window quit check showed an unreadable raster artifact during
unsaved-work review. Resizing restored the correct warning and the unsaved
Trim; Keep Editing preserved the session, and a subsequent explicit discard
closed only synthetic work. Disk sources and saved results remained unchanged.
Two matching installed-baseline build 157 checks and three further candidate
checks (without Join, after visiting Join, and after a repeated full join)
showed the normal warning. This does not establish the artifact's cause or a
rendering fix. Independent guest screencapture was unavailable; the failure
was retained rather than bypassing capture permissions. This display
observation remains a validation limitation, separate from the exact data and
ownership checks. The installed application was still baseline 157 during
these comparisons.

## Full release rerun

[Machine-readable results](2026-10-19-join-review-acceptance.json) retain source,
probe, harness and binary hashes, oracle hashes, exact metrics and environment.
All four full cases pass on clean source
2d16932d4b960c9237a437f91ce55a8d8c77b6a7, release executable SHA-256
420334748d9a78a00a0d9eec8a7ed36b9ddbe41b149ab0e7bafcb668c3f7c43a.

| Case | Result rows / bytes | Engine seconds | Peak RSS bytes | Tracked peak temporary bytes |
|---|---|---:|---:|---:|
| wide complete | 32 / 32921700 | 0.650173 | 49840128 | 117741812 |
| skew complete | 1572864 / 814743616 | 6.099969 | 5013504 | 1756066368 |
| large complete | 73728 / 9674630442 | 170.905054 | 24084480 | 38692391148 |
| large cancel | Cancelled / no result | 93.815149 | 23937024 | 28964061366 |

The wide case writes all 65,536 output columns using sixteen key pairs.
The skew case expands 1,024 by 1,536 matches into 1,572,864 results. Every byte
of both results matches the independent oracle.

The larger-than-RAM case reuses earlier verified, real non-sparse inputs through
hardlinks, with fresh output and private spills. Its 19,330,006,916 logical and
19,337,994,240 allocated input bytes both exceed the VM's 16 GiB RAM. The complete
73,728-row, 9,674,630,442-byte result matches the independent SHA-256. Reusing
these inputs does not allocate another copy of their extents. Whole-fixture du
samples include those shared inputs, rather than new allocation alone.

Large-file cancellation after the 1,000-row threshold takes 0.203503792 seconds
from request through cleanup, retains both input hashes, and leaves no result
or owned unpublished data. Its allocated-fixture samples include the prior
completed result. A separate full skew cancellation also passes, taking
0.024837375 seconds after its request.

An earlier eight-row large-record smoke cancellation finished before the probe
observed the requested cancellation point. The harness returned exit 1. That
failed observation is retained in the JSON and is not counted as successful
cancellation evidence; the full-workload checks above followed it. No assertion
was weakened to turn that smoke run green.

The harness builds before the timed probe. No other build or test ran during
these measurements. RSS, tracked logical temporary-byte peaks and sampled
whole-fixture allocated bytes remain distinct; the
[original report](2026-10-19-join-acceptance.md) describes their limits.
These are synthetic workload measurements, not controlled performance
comparisons or an absolute process-memory guarantee.

The current harness builds its probe automatically from a clean checkout.
Use fixture folders outside the source checkout, for example:
~~~sh
python3 scripts/bench-join-acceptance.py --case wide --root /path/to/new/wide
python3 scripts/bench-join-acceptance.py --case skew --root /path/to/new/skew
python3 scripts/bench-join-acceptance.py --case large --root /path/to/new/large
python3 scripts/bench-join-acceptance.py --case large --root /path/to/new/large --reuse --cancel-after-rows 1000
~~~

Review, merge and clean installed-VM verification remain delivery gates. They
are recorded separately on #71; this report does not claim they have completed.
