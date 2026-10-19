# Saved-file join validation

Date: 2026-10-18. Initial saved-file slice of [#71](https://github.com/danchamorro/quarry/issues/71), tracked in [#120](https://github.com/danchamorro/quarry/issues/120).

## Release probe

- VMPal macOS VM, Apple M3 Max (Virtual), 16 GiB RAM; macOS 27.0.1 (26A434); Rust 1.88.0.
- Release build of the public engine example, source revision 8a08066377b10e9f27b1fb0ac20d861f518dd757.
- Main: 200,000 rows with a six-digit key in permuted source order and a 251-byte text field. Additional: 100,000 odd-numbered keys with a 71-byte text field. Both are headered UTF-8 comma-delimited files with LF separators.
- Inputs total 59,700,017 bytes. Expected matches: 99,999. Expected result: 200,000 rows and 59,899,960 bytes in original Main order.
- Two sequential warm runs. Inputs were hashed before measurement; no cache eviction or concurrent build/test load. Timing includes the example's polling. RSS comes from /usr/bin/time -l. Temporary disk is the engine's tracked logical file-byte peak, including output staging, rather than an independent filesystem allocation sampler.

| Run | Total seconds | Maximum RSS bytes | Tracked peak temporary bytes |
| --- | ---: | ---: | ---: |
| 1 | 0.930623 | 52,084,736 | 201,599,883 |
| 2 | 0.852696 | 52,068,352 | 201,599,883 |

Both output files match this independently generated SHA-256:

81d4e60ecea77c50b7929d30c79eb7a1b6a7bcdcdad13917835225308cec83bd

The Python oracle enumerates original Main ordinals, computes each key as (ordinal * 73) % 200003, and appends the matching odd-numbered Additional row or two empty fields. It does not use Quarry's parser, comparator, or join implementation. Input SHA-256 hashes were unchanged after both runs; no private spill directories remained.

The source example can be run with four path arguments:

    cargo run -p quarry-core --release --locked --example join_saved -- MAIN ADDITIONAL OUTPUT TEMP

## Scope and remaining validation

This probe exercises only the first saved-file slice: headered UTF-8 files, one unique case-sensitive key, all Main rows, all columns, a new UTF-8 output, and original Main row order.

It is smaller than VM RAM and has no duplicate expansion. It does not establish the full #71 large-file acceptance. The larger-than-RAM, skewed-key, mixed-encoding, composite-key and complete lifecycle matrix remains required under the linked implementation tickets.

## Native desktop and regression checks

The candidate app (build 162, revision 8a08066377b10e9f27b1fb0ac20d861f518dd757, clean source) was packaged and driven through VMPal on the same VM. The installed main app remained build 157.

- Opened a separate document and made an unsaved cell edit.
- Opened Join Files, selected two saved inputs through native file dialogs, and selected their first-column keys.
- Analyze & Preview reported 3 Main rows, 2 Additional rows, 2 matching pairs, 1 unmatched Main row and 0 empty keys. The preview retained Main order.
- Chose a new destination through the native save dialog and created a 3-row, 73-byte output.
- Verified the complete output bytes against a handwritten oracle and confirmed both inputs and the separate document's on-disk bytes were unchanged.
- Opened the result in a new tab, edited a cell and undid the edit. The other tab retained its earlier unsaved value. Restored the synthetic edits with Undo and quit normally.

All 559 workspace tests, formatting, all-target/all-feature Clippy and the workspace release build passed. After the final UI explanatory text change, all 5 focused GUI workflow tests passed again. The 12 core join tests include a nested-loop oracle with forced small spills, repeated header names, empty/missing keys, strict text, source changes, corrupt frames, cancellation, hard-link rejection and private file permissions. GUI tests cover full-tab behavior, input/output reservations, the operation limit and close cleanup.

These checks establish the first saved-file workflow only. No installed production join feature or full #71 acceptance is claimed by this report.

## Review regressions

Two review findings were reproduced by failing tests before their fixes. A destination inside the owned spill workspace could be reported as saved and then removed during workspace cleanup; creation now rejects such destinations, including a symbolic-link alias to that directory. A failed cleanup could preserve an old app-quit request; both ordinary cleanup errors and a panicked cleanup worker now cancel that request, so a later dialog Close remains local.

After these fixes, all 562 workspace tests, formatting, all-target/all-feature Clippy and the release build passed. The earlier native workflow and release measurements above describe the stated candidate revision; these additional checks exercise the review fixes at the core and dialog-state boundaries.

The header-picker worker now uses a cancellable job and bounded reads instead of a synchronous Session open inside an uncancellable thread. Its test first demonstrated that cancelling a completed-but-unadopted header still populated the draft; the fixed dialog discards that result. Additional checks cover cancellation before inspection, cancellation between reads/seeks, delimiter detection, bounded UTF-8 labels, invalid headers, empty files and non-regular inputs. A current operating-system file read must return before the worker can observe cancellation.

All 565 workspace tests, all-target/all-feature Clippy, formatting and the workspace release build pass after this change. The native evidence above predates the header-worker change.

Further review regressions bind the selected column ordinals to an opaque file-version token from the header probe. Replacing or editing a source before analysis now requires selecting it again, so a reordered header cannot silently change the join key. Analysis/writing failures and successful-write cleanup warnings cancel deferred close/quit and remain visible for review. The non-Unix storage fallback now reuses the shared helper that inspects both temporary and output locations.

The stale-schema and worker-cleanup regressions failed before these fixes and pass afterward. All 567 workspace tests, formatting, all-target/all-feature Clippy and the release build passed in the macOS VM. The non-Unix storage branch was reviewed against the existing shared helper; no Windows runtime check is claimed.

Structured diagnostics now preserve the logical record number from JoinFile errors, matching the existing CombineFile handling. The new regression failed before the fix and passed afterward; all 568 base-branch workspace tests and all-target/all-feature Clippy passed.

## Destination retry and pinned-parent publication

The output preflight now rejects existing paths, including dangling symlinks, before consuming the analyzed plan. A GUI regression proves that the same retained preview can create a different fresh output after an existing destination is rejected; the existing file remains unchanged. The regression failed before the fix.

A subsequent CodeRabbit outside-diff finding exposed a time-of-check/time-of-use gap in the workspace guard. Two real-filesystem regression tests failed before the directory-pinning fix: retargeting a parent alias after validation produced a reported success whose result disappeared during workspace cleanup, and retargeting after staging stranded the private file. Both pass after the fix. Additional checks cover identity-based workspace rejection after an alias moves outside, cancellation and a racing existing destination after retargeting, cleanup failures with unrelated entries, inherited macOS ACL removal, and file-only drop-folder fallback.

All 575 workspace tests, formatting, all-target/all-feature Clippy and the release build passed in the VMPal macOS 27.0.1 (26A434) VM. The Windows handle-locking branch has source-level coverage and Windows-only tests, but was not executed here; Linux core CI is required separately. These are targeted boundary checks, not the complete #127 failure-injection matrix.

A direct release executable built from the uncommitted directory-pinning patch over 2dc04f1f679384f295353b4095c951ab712ddfe7 was driven natively in the VM on 2026-10-19 UTC. SHA-256: 59707861ae1ae04d28d76a8431bf25c21f05874bcf4a10e337e0ede1202d40b1. Both saved inputs were selected with native open dialogs. Analysis reported 3 Main rows, 2 Additional rows, 2 matching pairs, 1 unmatched Main row and a 74-byte result. A destination folder alias was selected with the native save dialog. The complete output matched the independent handwritten fixture (SHA-256 943a62f114511df4936e4109324105bae6ac3d9321d895c86c4613a90be548b0), both input byte strings remained unchanged, and no private output staging remained. Open Joined File displayed 3 rows and 4 columns in Main order; Quarry quit normally with the clean synthetic document. The installed VM bundle remained build 157, and the host app was untouched. The adversarial alias timing is established by deterministic core tests, not by this native happy-path check.


## Current-name, portability and source-version regressions

A real macOS regression first reproduced a renamed destination folder (and a renamed ancestor) returning the unrelated replacement created at its old name. The fixed path resolution follows the retained directory handle and identifies the actual saved result. The check passes while preserving the unrelated replacement and cleaning the original staging.

The Linux unsupported-rename check failed with EINVAL before the fallback and passes afterward. This injects the syscall result at the platform boundary and exercises real descriptor-relative hard-link publication, an existing destination, cleanup and propagation of unrelated errors on the macOS filesystem. It does not claim a live NFS measurement.

[Windows failing-before run](https://github.com/danchamorro/quarry/actions/runs/39200992580) uses unchanged production code from 422b1cf and the same new native regression assertions. Both same-size/same-time source replacement and restored-mtime rewrite were incorrectly accepted. The existing directory-locking test also failed for renaming the selected leaf; its zero-access handle did not establish the intended delete-sharing exclusion. All three failures were runtime assertions, not compilation errors. The corrected Windows implementation and nonrecursive cleanup require their new CI run to pass; no local Windows GUI/build execution is claimed.

After the current-name, Linux fallback and shared Windows guards were added, all 577 macOS workspace tests, formatting, all-target/all-feature Clippy and the workspace release build passed in the configured VMPal VM. The earlier native happy-path evidence remains tied to its recorded executable. Native folder-rename timing is tested deterministically at the real core publication boundary.
