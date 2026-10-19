# Current-document join workflow evidence

Environment: VMPal macOS VM 1, macOS 27.0.1 (26A434), Apple Silicon,
Rust 1.88.0. All source files and edits in these checks are synthetic.

The complete combined workspace passed 651 tests. The egui harness uses real
AccessKit actions for open-document selection, Analyze & Preview, creation,
reset/close, and opening the ordinary editable result.

Verified paths include:

- a reordered private Main generation, current header/cell edits in both open
  inputs, and a changed grid column order; exact output uses current physical
  row order and original source-column identities;
- browse aliases resolving to current values, stale schema refresh, data-only
  key/value changes preserving the chosen output layout, and inherited
  Windows-1252/tab input with unsaved values requiring UTF-8 output;
- input edit/save/cleanup/reload/close guards during a retained preview, unchanged
  histories, an independently editable unrelated tab, and lease release after
  reset, close, and successful creation;
- idle/unfiltered requirements, same-document rejection, oversized drafts
  retained on capture failure, rejection of private history-file inputs, and
  late source-selection actions leaving an existing preview unchanged.

Formatting, all-target/all-feature Clippy with warnings denied, all 17 macOS
packaging self-tests, and macOS app packaging pass.

## Native VM workflow

Tested the local candidate at revision
`92e61e60510ed98fb47a490ea3324cef158ab842`, app build 194, on the VM above.
The package reports `dirty` because of the preserved untracked `plans/` directory;
tracked source was clean. This candidate was launched from the package directory,
and was not installed over the VM or host application.

Through native controls, opened two synthetic CSVs, edited a value in each without
saving, selected both open documents in Join Files, ran Analyze & Preview, chose a
new destination through the native save panel, created the result, and opened it
in a third tab. The complete output was compared byte-for-byte with:

```csv
id (Main),name,id (Additional),city
2,Native edited,2,Native City
1,Alpha,1,Seattle
```

The result uses UTF-8 without BOM and LF record separators. Editing a result cell
changed its ordinary dirty state. Both original source hashes remained unchanged:

- Main: `2141009aa94201c20099f6d9b092631c87f683e6cba2fc38d3e81de50f9a259e`
- Additional: `ba7678f96f8fdfd0d2954e9637049656dc03ff68081aee44432dfae34395adec`

Quit prompted for unsaved work. Keep Editing cancelled the first quit; a subsequent
quit reviewed each synthetic tab. Discarded only these test edits, then verified
the application exited, both inputs retained their hashes, and the saved joined
result retained its original bytes.

This proves the small native workflow. Background final-document cleanup and its
fault/quit coordination, platform lifecycle coverage, and the full large-file
acceptance matrix remain separate gates.


## Final document cleanup regression

The new quit regression failed before the lifecycle change with `quit must wait
for final document cleanup`: native close had been authorized before final
private-storage destruction. It passes with the background retirement worker.
A real foreign file is preserved, the cleanup warning remains visible, and
application quit is not authorized when cleanup fails.

Additional checks gate the real retirement closure to verify visible progress
until completion, final removal before native close, stale close/Keep Editing
actions, source and moved-root path reservations, independent editing in another
tab, and preservation of both dirty tabs when thread creation fails. Existing
save/close, recovery, filtered-tab and structural-working-file checks now wait for
the real retirement worker and retain their exact data/removal assertions.
These checks extend the initial 651-test evidence; final combined counts and CI
are recorded after integration. They do not replace native testing of the new
cleanup UI or the remaining full failure and scale matrix.


## Native final-close failure check

The rebased candidate at `0e9fde29b2c1412e267cbe6af1774815a60359f3`, app build
197, passed 655 workspace tests, formatting, Clippy and the 17 packaging
self-tests, and was packaged successfully in the same VM. It was launched from
the package directory without changing the installed application.

Using native controls, trimmed a synthetic column to create a private working
generation. Added a synthetic foreign sentinel to that owned directory, requested
quit and approved discarding only the synthetic edits. Quarry removed its own
generation, preserved the foreign file and original CSV bytes, displayed the
cleanup failure, and remained open. A subsequent explicit quit closed the app.
The original CSV SHA-256 remained
`48927172e4a793a515326102e4ebd6e60828395bb7f51fcc2849062897ea9aac`.

This exposed an actionable-diagnostic gap: the warning did not identify the
remaining folder. A new folder-path assertion failed before the diagnostic fix
and passed afterward. Cleanup reports now include the actual captured folder
(or its original diagnostic path if it cannot be located) and tell the user to
review remaining files before deleting anything. The longer native warning's
layout remains part of the final UI matrix.

A further portable public-job test uses a private generation as the next job's
source, releases the document's owner, then exercises completion, cancellation
and dropping the last job. All three paths remove the owned root, retain original
source bytes, release ownership and report no cleanup errors in macOS. The
existing Windows `working_storage::tests` CI step also runs this case; native
Windows results are required before clearing that platform lifecycle check.
