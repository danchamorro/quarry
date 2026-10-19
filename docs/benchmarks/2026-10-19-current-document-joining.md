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

Formatting and all-target/all-feature Clippy with warnings denied pass. Native
VM evidence, release/package checks, final close/quit cleanup fault coverage and
the full large-file acceptance matrix remain separate gates.
