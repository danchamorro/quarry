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

## Amendment (2026-11-22): Whole-row duplicate target (`AllFields`)

Duplicate detection now supports `DuplicateTarget::AllFields` in addition to
`DuplicateTarget::Columns(Vec<usize>)`.

### Whole-row semantics
- Compare the complete decoded field tuple of each effective (edited) data row
  together with its explicit field count (`a,b` with 2 fields differs from
  `a,b,` with 3 fields, while `"a",b` matches `a,b`).
- Exclude the file BOM once at byte 0 only, exclude the header row when
  present, exclude outer `CRLF`/`LF` record endings, and decode UTF-16 inputs
  to UTF-8 before comparison.
- ASCII case-insensitive folding (`CaseSensitivity::Insensitive`) folds ASCII
  letters only; whitespace and non-ASCII bytes remain exact.

### Single-copy frame layout and collision safety
- Storing a length-framed copy of every decoded field in the run key would
  double temporary frame payloads across merge generations.
- Instead, `DuplicateTarget::AllFields` stores each effective record once per
  frame alongside a fixed 24-byte key: `(field_count: u64 BE, dual_hash_128)`
  computed over the normalized decoded field tuple.
- Runs sort and merge strictly by `(24-byte key, ordinal)`. Whenever adjacent
  entries share a 24-byte key during run flushes, in-memory deduplication,
  intermediate merge passes, or `retain_first_runs`, Quarry compares the
  decoded field tuples directly against the distinct survivors retained for
  that key bucket (with a raw-byte fast path after stripping outer line
  endings). Even under an adversarial 128-bit hash collision, this guarantees
  zero false positives, zero false negatives, and stable first-occurrence
  retention.
- Temporary disk preflight (`estimate_duplicate_target_temporary_bytes`) uses
  `2 * E + 112 * R` for `AllFields` (two generations of single-copy records
  plus 24-byte keys and frame headers) rather than the `4 * E` two-copy
  selected-column allowance.
