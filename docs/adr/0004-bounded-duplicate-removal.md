# ADR 0004: Reuse external sorting for duplicate removal

**Status:** Accepted
**Date:** 2026-09-05

## Context

Pre-beta priority 3 requires selected-column matching, a count before removal,
the first occurrence in current row order, unsaved values, and bounded memory
for files larger than RAM. The retained document must remain recoverable through
the existing working-copy history and save workflow.

## Decision

Reuse the existing external-sort scanner, runs, merge heap, and guarded output.
Sort length-framed decoded field tuples with source ordinals as stable ties.
Retain the first record for each tuple, then sort retained records by ordinal.
This preserves row contents and relative order without storing all keys in RAM.

Keep the finished output as a private candidate while the user reviews the
extra-row count. Block edits during review and install only on explicit removal.
Cancel deletes the candidate. Recheck source stamps and prepare the candidate
before consuming the previous document's history.

An in-memory set would grow with unique values. A separate disk database or
partitioning engine would duplicate infrastructure already present. Repeating
analysis after confirmation would add a full scan and could produce a different
result from the reviewed count.

## Consequences and validation

Finding duplicates consumes temporary disk space and performs two orderings,
even when the user only reviews the count. Memory is bounded by run, record,
and merge payload limits. This phase does not add the temporary-location and
free-space controls reserved for priority 4.

The [validation report](../benchmarks/2026-09-05-find-remove-duplicates.md)
records exact retained bytes, cancellation, cleanup, source preservation,
and measured memory and disk use on the deterministic 1 GB workload.
