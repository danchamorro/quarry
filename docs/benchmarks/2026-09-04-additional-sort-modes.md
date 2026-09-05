# Additional sort modes validation: 2026-09-04

Character count, Word count, Shuffle, and Reverse reuse the existing external
sort and guarded working-copy workflow. This report extends the
[numeric sorting validation](2026-09-04-numeric-sort.md).

## Dataset and environment

The numeric report's reproducible generator was used unchanged: 1,000,000,148
bytes, 5,408,918 data rows, and three columns (amount, original row ID, note).
The source contains signs, decimal/exponent representations, blanks, whitespace,
equal keys, and quoted multiline notes. Source SHA-256:
`fb160ff92e2f8efcc759fd60dd28e786820613068be312f2834a8449e6b1366d`.

Apple M3 Max, 16 cores, 128 GiB RAM; macOS 26.6.2 (25G83), Rust 1.88.0,
locked release builds, warm caches without a cache purge. This is the local
`codex/numeric-sorting` candidate based on main `7a8fd9c`. Measurements cover
1 GB; they do not claim new 12 GB or 50 GB results for these modes.

## Results

| Mode | Column | Worker seconds | Peak RSS | Peak temporary bytes | Output FNV-1a 64 |
|---|---|---:|---:|---:|---|
| Character count, shortest first | amount | 5.760 | 29.31 MiB | 2,173,085,653 | `37b80b355bef48df` |
| Word count, fewest first | note | 6.286 | 29.36 MiB | 2,173,085,653 | `45450eca80492ce1` |
| Reverse | All rows | 5.013 | 25.16 MiB | 2,173,085,653 | `3df88d28bd68a0c1` |
| Shuffle, seed 7 | All rows | 6.914 | 29.39 MiB | 2,173,085,653 | `d4083504cda0cf43` |

Time is worker time only, excluding subsequent CLI and independent validation.
The temporary-disk allowance was 4,411,078,360 bytes for every mode. All jobs
used two merge passes. Counts and row-order keys stay at eight bytes per row
in the bounded run and merge buffers.

The preserved numeric-only candidate took 6.458 seconds at 28.56 MiB RSS;
the expanded candidate took 6.586 seconds at 29.05 MiB RSS on the same Number
sort. Their outputs were byte-identical. These single-run observations are
compatibility evidence, not a speed claim.

## Correctness and cancellation

The CLI verified complete row counts and the exact raw header; the worker
verified its bounded dual record fingerprint and stable key ties. For Reverse,
the CLI also compared every output batch against reversed source rows.

An independent CSV scan checked every output amount and note against values
regenerated from the original row ID. A fixed 676,115-byte bitmap checked that
each of the 5,408,918 row IDs appeared exactly once. The same scan checked
ascending character/word counts, increasing source IDs within equal counts,
and exact descending source IDs for Reverse. Shuffle used seed 7 and preserved
the full record set. The source SHA-256 was unchanged.

Shuffle cancellation requested at 64 MiB stopped after 65 MiB, with 2.285 ms
latency and 29.02 MiB peak RSS. No destination or unpublished sort run remained.

Focused tests cover both count directions, Unicode scalar values, combining
marks, Unicode whitespace, blanks/missing values, invalid UTF-8 rollback,
sparse edits, forced multipass runs, header and headerless BOM preservation,
missing final newlines, exact Reverse, deterministic same-seed Shuffle,
different-seed permutations, and invalid direction rejection. An independent
key check covered 2,005 Unicode strings plus 1,000 Reverse/Shuffle ordinals.

All 248 workspace tests passed, including accessibility-driven clicks through
every new choice and direction, mode-specific completion messages, source
preservation, and Undo/Redo. Date and time sorting is planned in Phase 6D and
is not implemented in this candidate. The local app bundle builds and its
signature verifies. Native visual review of the expanded dialog could not be
completed because the desktop-control tool repeatedly timed out in the file
picker; the automated desktop interaction regressions above passed.

## Reproduction

Use the numeric report's generator to create `numbers.csv`, then build:

```bash
cargo build --workspace --release --locked
target/release/quarry-bench sort-save-as numbers.csv characters.csv \
  --mode characters --column 1 --order asc --header first-row --cache-state warm
target/release/quarry-bench sort-save-as numbers.csv words.csv \
  --mode words --column 3 --order asc --header first-row --cache-state warm
target/release/quarry-bench sort-save-as numbers.csv reversed.csv \
  --mode reverse --header first-row --cache-state warm
target/release/quarry-bench sort-save-as numbers.csv shuffled.csv \
  --mode shuffle --seed 7 --header first-row --cache-state warm
```

Use a new destination for each run. Add `--cancel-after-bytes 67108864` to the
Shuffle command for the cancellation check. CLI seeds repeat a shuffle within
the same build; the standard library hash algorithm is not a cross-version
file-format contract. The desktop generates a fresh seed for each Shuffle.
For independent checks, parse CSV records, use `len(amount)` for this fixture's
character count and `len(note.split())` for its word count, and track source IDs
with a bitmap. Regenerate the expected fields using the numeric fixture formula.
