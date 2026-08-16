# Row density validation

## Decision

Ship the compact viewer grid. On the same maximized Mac display used for the
product comparison, Quarry dynamically fitted 42 visible data rows instead of
23 while keeping the existing font sizes, toolbar, header, selection treatment,
and bounded viewport model. The EmEditor screenshot showed 45 data rows and
remains a visual density reference, not a performance gate.

## Environment and dataset

- Date: 2026-08-16
- Hardware: Apple M3 Max MacBook Pro, 16 CPU cores, 128 GB RAM
- OS: macOS 26.6.1 (25G76), arm64
- Rust: 1.97.1
- Build: `cargo +1.97.1 build --workspace --release`
- Cache state: unknown; this validation measures layout and interaction, not
  file-read performance
- Reference CSV: `605Lending080626.csv`, 3,438,846 bytes, modified
  2026-08-07 13:24:52 EDT
- Reference CSV SHA-256:
  `d42f8a377f0c559324f56c821cdc2acc6f6ce0cecc4c01d57aa4daa6644ea36e`
- Supplied outer-window screenshot: 3,456 by 2,168 Retina pixels, including the
  64-pixel macOS title bar
- Automated inner viewport: 1,728 by 1,052 logical points after excluding that
  title bar

## Implementation

Only the data grid became denser. Rows use a 17-point height with no vertical
gap inside the table, and selectable cells use compact button padding. The
13-point monospace data font, 30-point header height, toolbar, status area, and
global spacing remain unchanged. The bounded bootstrap target increased to 40
rows; each rendered frame still calculates its visible count from the available
grid height. The active buffer retains only visible rows plus two overscan rows
on each side.

## Results

| Check | Result |
|---|---|
| Quarry before | 23 visible data rows in the supplied maximized screenshot. |
| EmEditor reference | 45 visible data rows, rows 2 through 46. |
| Quarry after | 42 visible data rows, rows 1 through 42. |
| Legibility | No clipped or overlapping text was visible; headers and striped rows remained distinct. |
| Selection | A selected cell remained clearly visible in the compact grid. |
| Page Down | Rows 1 through 42 advanced to rows 43 through 84. |
| Page Up | Rows 43 through 84 returned to rows 1 through 42. |
| Continuous scroll | A Computer Use scroll moved the viewport to rows 64 through 105 without paging controls. |
| Accessibility | The full-app regression exposes row 40 and its first cell as labelled buttons. |
| Bounded buffer | The regression requires buffered rows to remain at or below visible rows plus four overscan rows. |

The automated regression renders the complete application, including the
toolbar and status area, rather than testing the grid in isolation. This keeps
the evidence-scoped minimum of 40 rows tied to the actual maximized viewer
layout without fixing the production row count.

## Validation

The final source passed:

```bash
cargo fmt --all -- --check
cargo +1.97.1 test --workspace
cargo +1.97.1 clippy --workspace --all-targets --all-features -- -D warnings
cargo +1.97.1 build --workspace --release
git diff --check
```

The workspace run passed 45 tests, including 21 `quarry-egui` tests.

## Limits

This slice does not add a density preference, shrink global fonts, change the
two-row overscan, or redesign the toolbar. Computer Use validated the reference
CSV. No fresh 12 GB visual validation completed; the automated regression
verifies the bounded viewport buffer.
