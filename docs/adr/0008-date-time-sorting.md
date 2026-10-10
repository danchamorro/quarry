# ADR 0008: Explicit date and time sort formats

Status: Accepted for [issue #56](https://github.com/danchamorro/quarry/issues/56).

## Context

Text order does not consistently order dated records chronologically. Ambiguous
dates must not depend on locale or the computer timezone, and huge files need
the existing external-sort resource and safety guarantees.

## Decision

Add Date / time to the main egui Sort window with Oldest first / Newest first
and five explicit formats: YYYY-MM-DD, MM/DD/YYYY, DD/MM/YYYY, timezone-free
YYYY-MM-DD HH:MM:SS (space or uppercase T), and an RFC 3339 subset with uppercase
T and Z or signed HH:MM offsets. Require a selected format in the validation CLI.

Validate exact field widths, Gregorian years 0001–9999, calendar days, and
00:00:00–23:59:59 clock values. Timestamps accept optional 1–9 fractional digits.
Accept offsets within 23:59. Reject leap seconds, unknown offset -00:00, timezone
names, two-digit years, automatic mixtures, and extra fractional precision.
Trim surrounding ASCII whitespace for comparison only. Empty/missing fields
sort first oldest and last newest. Invalid nonblank values abort with data row,
column, chosen format, and a corrective explanation; retain the source and
current working version.

Use bounded fixed-format byte parsing and a 12-byte big-endian key of signed
whole seconds (sign bit flipped) plus nanoseconds. Clock formats never apply a
timezone; offset formats normalize to UTC, including instants beyond the local
four-digit date boundary. Stable source ordinals break equal-key ties in either
direction. Row bytes and original date spellings remain intact.

The Gregorian day calculation counts preceding complete years and leap years,
then preceding month lengths. Four-digit input years bound every arithmetic
operation. This small fixed grammar adds no date library, locale tables, timezone
database, or proportional key storage. General-purpose date parsing and automatic
inference were rejected because the initial format set is explicit and narrow.

Reuse the external-sort workers, source checks, storage checks, cancellation,
index handoff, and working-copy Undo/Redo. Parsing remains frontend independent.
No date coercion, date filtering, or multi-column sorting is introduced.

## Consequences and validation

Users must select a matching format; mixed or unsupported input needs correction
before sorting. Offset spelling may differ while comparison keys are equal.
Regression checks cover ambiguity, Gregorian leap centuries, nanosecond order,
offset day/year boundaries, invalid values after spill, stable multipass ties,
unsaved edits, quoting/multiline rows, UI accessibility, history, and Save As.
Native VMPal checks and large-file measurements belong in the linked validation
report before merge.
