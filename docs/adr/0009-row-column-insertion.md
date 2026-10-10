# ADR 0009: Materialize inserted rows and columns into guarded working files

## Status

Accepted for [issue #73](https://github.com/danchamorro/quarry/issues/73).

## Context

Users need editable blank rows and columns in an existing large delimited file.
Insertion must include pending values, preserve the saved source, and remain
bounded for arbitrary counts. Filters and view-only column layouts make visual
positions ambiguous. Empty and ragged files need explicit behavior.

## Decision

Expose position/count dialogs through the Insert toolbar menu and numbered
row/column context menus. Positions identify physical data-row boundaries
(excluding headers) or numbered file-column boundaries, including the end.
Require a completed index, no filter and no conflicting operation. A prior sort
defines the current physical row order. Materialization resets column visibility
and order, as existing structural operations do.

Rows use a cancellable count pass followed by a guarded save-worker copy.
Stream untouched source records directly from a fixed read chunk, including
records larger than the rewrite limit. Retain one bounded record only for
pending value edits and reuse a blank row buffer; write the count without
allocating a list of rows. Preserve raw records except pending edits, BOM
relocation before a first headerless row, and a required final separator.
Use the first record's newline style, with CRLF when needed to preserve a final
literal carriage return. Inserting before record zero determines its ending
with a bounded scanner peek and rewind, without retaining that record. Enforce the output record limit after adding a separator.

Columns use the existing streaming field transformation. Insert blank values and
optional header names, pad short records to the known source width, and retain
late extra fields. Reject width or record-limit overflow without publication.
Use the existing 65,536-column limit. Known width remains the viewer's schema;
this feature does not add a whole-file maximum-width discovery pass.

Header-only files support both operations. Headerless files do not gain a header.
Empty headerless files first gain blank one-column rows; reject column insertion
until at least one record can represent the schema. An empty file opened with a
forced header must be reopened as headerless.

Use existing private working files, storage preflight, source identities,
cancellation cleanup, and one adjacent structural Undo/Redo version. Sparse
edits are materialized with the operation. Save, Save As and Discard keep their
existing safety boundaries.

## Alternatives

- Sparse logical inserted rows would require teaching indexing, navigation,
  filters, sorts, export and history about synthetic identities. Materializing
  fits the existing engine and ordinary editable-grid contract.
- Inserting into filtered coordinates or displayed column order creates unclear
  targets; explicit physical boundaries avoid implicit data remapping.
- Holding all inserted rows or all source records violates the large-file budget.
- Rejecting every ragged file would make ordinary structural editing less useful.
  Padding short records and preserving extra fields avoids silent loss.

## Consequences

Row insertion reads the source twice and writes a new working file. Column
insertion rewrites field serialization, retaining decoded values and record
endings. Temporary disk grows with output size, while memory grows with record
size and schema width. History remains bounded to one adjacent whole-file change.
Native checks and representative resource measurements are recorded in
[the insertion validation report](../benchmarks/2026-10-10-row-column-insertion.md).

The public core RowInsertion.position is a zero-based physical record boundary,
including the header: 1 inserts before the first data row with a header, 0 does
so without a header, and the total source record count appends. The desktop
converts its one-based data-row control to this coordinate system. The count
pass validates that boundary; only transformations or replacement pass the full
record count into the storage estimator. Row insertion uses the same sparse-edit
allowance as desktop preflight, plus its blank-row allowance.
