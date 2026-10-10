# ADR 0006: Validate matching files before streaming a new combined file

## Status

Accepted for the implementation of [issue #54](https://github.com/danchamorro/quarry/issues/54).

## Context

Users need to combine any selected set of equally shaped files into one file.
They define file order and expect all rows to survive. Compatibility errors
can occur near the end of an input much larger than RAM. The open document may
also contain unsaved edits that should not be implicitly included or lost.

## Decision

Use a standalone Combine Files dialog and an engine pipeline with an explicit
check followed by an explicit new-file write. The check scans every record,
compares the delimiter, exact decoded header names/order and record widths,
and records counts, exact output size and source stamps. Header mode applies
to every input and is selected explicitly. Reject empty files, repeated paths,
ragged widths and malformed records. Header-only inputs are valid.

The writer opens one file at a time and retains a fixed read buffer and one
record, bounded by the existing 64 MiB record limit. It copies raw records,
keeps the first header once, removes later encoding BOMs and terminates a final
record lacking LF. A list of input identities grows with file count, not row
count. Recheck all input identities at publication through the existing atomic
output target. Capacity checks and unpublished-output cleanup use the same
storage and RAII machinery as other writers.

The active document is not an implicit input. Only selected saved files are
read. Open the completed result explicitly through the existing unsaved-work
guard. No cross-file Undo history is introduced; normal editing history begins
when the result is opened.

## Alternatives

- Appending to the current document requires ambiguous handling of its
  unsaved values, view order and structural history. It does not match the
  agreed selection-to-new-file workflow.
- Writing during the compatibility check avoids a second read but can spend
  substantial disk space before discovering an incompatible final input and
  cannot present a fully checked summary before the user commits to writing.
- Loading all inputs or records into memory violates the large-file budget.
- Re-serializing every field adds copying and can change valid formatting
  unnecessarily. Matching delimiters permit raw-record copying.

## Consequences

Successful combination reads the input data twice. This buys complete
validation before writing and exact storage estimates. Input mutations after
validation require a recheck. Header names are case- and whitespace-sensitive;
there is no automatic mapping, deduplication or width repair. Very large
individual records retain the existing limits. Performance and native
validation evidence is recorded separately from this decision.
