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
