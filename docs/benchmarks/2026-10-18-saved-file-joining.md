# Saved-file join validation

Date: 2026-10-18. Initial saved-file slice of [#71](https://github.com/danchamorro/quarry/issues/71), tracked in [#120](https://github.com/danchamorro/quarry/issues/120).

## Release probe

- VMPal macOS VM, Apple M3 Max (Virtual), 16 GiB RAM; macOS 27.0.1 (26A434); Rust 1.88.0.
- Release build of the public engine example, source revision 7cea944a91475764f05327131fd3994c0d574d0a.
- Main: 200,000 rows with a six-digit key in permuted source order and a 251-byte text field. Additional: 100,000 odd-numbered keys with a 71-byte text field. Both are headered UTF-8 comma-delimited files with LF separators.
- Inputs total 59,700,017 bytes. Expected matches: 99,999. Expected result: 200,000 rows and 59,899,960 bytes in original Main order.
- Two sequential warm runs. Inputs were hashed before measurement; no cache eviction or concurrent build/test load. Timing includes the example's polling. RSS comes from /usr/bin/time -l. Temporary disk is the engine's tracked logical file-byte peak, including output staging, rather than an independent filesystem allocation sampler.

| Run | Total seconds | Maximum RSS bytes | Tracked peak temporary bytes |
| --- | ---: | ---: | ---: |
| 1 | 1.046636 | 52,117,504 | 201,599,883 |
| 2 | 0.977959 | 52,101,120 | 201,599,883 |

Both output files match this independently generated SHA-256:

81d4e60ecea77c50b7929d30c79eb7a1b6a7bcdcdad13917835225308cec83bd

The Python oracle enumerates original Main ordinals, computes each key as (ordinal * 73) % 200003, and appends the matching odd-numbered Additional row or two empty fields. It does not use Quarry's parser, comparator, or join implementation. Input SHA-256 hashes were unchanged after both runs; no private spill directories remained.

The source example can be run with four path arguments:

    cargo run -p quarry-core --release --locked --example join_saved -- MAIN ADDITIONAL OUTPUT TEMP

## Scope and remaining validation

This probe exercises only the first saved-file slice: headered UTF-8 files, one unique case-sensitive key, all Main rows, all columns, a new UTF-8 output, and original Main row order.

It is smaller than VM RAM and has no duplicate expansion. It does not establish the full #71 large-file acceptance. The larger-than-RAM, skewed-key, mixed-encoding, composite-key and complete lifecycle matrix remains required under the linked implementation tickets.

Native desktop evidence and the final regression count will be added before submitting #120.
