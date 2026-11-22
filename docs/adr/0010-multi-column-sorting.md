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
header preservation, numeric/date types, and stable ties. Additional checks cover cancellation, key limits, storage estimates, GUI
controls, and Save As. Reproducible VM benchmark evidence accompanies the pull
request.

## Amendment (2026-11-22): Natural sort mode and key-headroom bounds

`SortMode::Natural` is added for both single-level and multi-level `SortPlan`
sorts.

### Natural comparison semantics and wire encoding
- Contiguous ASCII digit runs (`0`–`9`) compare by unsigned integer magnitude
  (`v1.9 < v1.10`). Signs, decimal points, and non-ASCII Unicode digits are
  treated as ordinary non-digit bytes; whitespace is exact.
- `CaseSensitivity::Insensitive` folds ASCII letters only; leading zeros in a
  digit run compare equal (`02 == 2`, `000 == 0`), with ascending source
  ordinals breaking ties stably in both ascending and descending directions.
- Each key uses a compact prefix-orderable encoding:
  - Non-digit bytes (never in `0x30..=0x39`) are emitted verbatim (ASCII
    lowercased when insensitive).
  - Each ASCII digit run emits tag `0x30`, followed by compact significant-digit
    length (`0x00` for all-zero runs, `0x01..=0xFE` for 1–254 significant
    digits, or `0xFF` + `u32 BE` length for 255+ significant digits), followed
    by the significant digits themselves.
- Multi-level `Natural` components reuse the existing `0x00 -> 0x00 0xFF`
  escaping, `0x00 0x00` terminator, and bitwise complement for descending order,
  preserving exact single-level and multi-level equivalence.

### Updated headroom and disk preflight
- Single-level `Natural` keys expand to at most `2 * field_len + 1` bytes
  (worst case: alternating single digits and non-digits such as `1a1a1`).
- Multi-level NUL-escaped `Natural` components expand to at most `3 * field_len`
  bytes excluding the 2-byte component terminator (worst case: alternating NUL
  and `'0'` bytes).
- `maximum_sort_key_bytes` is raised from `2 * max_record_bytes + 352` to
  `3 * max_record_bytes + MAX_SORT_KEYS * (2 * NUMERIC_KEY_OVERHEAD + 2)`
  (`3 * max_record_bytes + 352`). With the 64 MiB record limit and 16 levels,
  the worst-case 2-way merge retained payload (`max_record_bytes + 3 * max_key`)
  remains strictly bounded below 641 MiB, independent of source file size.
- `estimate_sort_plan_temporary_bytes` inspects the plan's modes: plans without
  `Natural` keep the existing `4 * E` / `6 * E` allowances; single-level
  `Natural` uses `6 * E + 52 * R` and multi-level plans containing `Natural`
  use `8 * E + R * (48 + key_count * 44)`.
