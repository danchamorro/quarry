# ADR 0010: Stable multi-column sorting

Status: Accepted for implementation of [#64](https://github.com/danchamorro/quarry/issues/64).

## Context
The external sorter already supports exact per-column comparisons, private
working copies, stable source ordinals, cancellation, and structural history.
Users need ordered tie-breakers with independent comparison settings. Duplicate
detection shares run I/O and must retain its current comparison behavior.

## Decision
Keep SortSpec as one level and accept SortPlan at the existing sorting entry
points. A plan has 1–16 distinct columns. A single Shuffle or Reverse remains a
whole-row operation. Single-level keys retain their existing representation.

For multiple levels, encode each existing comparison key as a byte-ordered
component: escape NUL as 00 FF, terminate with 00 00, and complement the
whole component for descending order. This includes the terminator so prefixes
reverse correctly. Concatenate components in priority order and retain the
source ordinal as the final ascending tie-breaker. The shared external merge
and duplicate comparators do not change.

Distinct columns bound raw field bytes by the maximum record size. A composite
key is bounded by twice that size plus 22 bytes per level for numeric metadata
and terminators. Run sizing uses actual key bytes. The disk allowance covers
two generations of framed runs, including worst-case escaping. Merge fan-in
uses actual maximum key width: it retains the existing 256 MiB payload budget
unless two heads, a pending key, and one record require more. With the 64 MiB
record limit and 16 levels, the worst-case retained payload remains below
449 MiB, independent of source file size. Metadata and I/O buffers are additional.

## Alternatives
Repeated full-file sorts would repeat I/O and complicate progress, cancellation,
and history. Allocating complete rows in memory would violate the large-file
contract. Parsing a tuple inside every heap comparison would add another
comparison and serialization path to the shared sorter.

## Consequences and verification
The GUI exposes ordered levels with independent type, direction, case, and
date-format controls. The entire sort produces one undo step. Invalid values at
any level abort the operation; missing values retain existing semantics.

Regression tests compare encoded order against an independent tuple comparator
for prefixes, NUL, high bytes, and mixed directions. A forced multi-pass test
compares decoded field order against an independent oracle and covers edits,
header preservation, numeric/date types, and stable ties. Additional checks
cover cancellation, key limits, storage estimates, GUI controls, and Save As.
Reproducible VM benchmark evidence accompanies the pull request.
