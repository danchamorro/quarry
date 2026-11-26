# Phase 5A-6 Adaptive Word Wrap Benchmark Report (2026-11-26)

This report records the pre-UI empirical benchmark for **Phase 5A-6: Adaptive Word Wrap** (`C16`, `T15`), testing whether Quarry's fetched-row wrapped-layout measurement scales with **total file size** or strictly with **visible viewport text**, and verifying worst-case latency under hostile multi-megabyte single-line cells.

## Environment & Command

- **Venue:** `host-bare-metal` (headless `quarry-workbench` example; desktop app verification runs separately in VMPal VMs)
- **Hardware:** MacBook Pro, Apple M3 Max, 16 cores, 128 GiB RAM, macOS `arm64`
- **Command:** `cargo run --release -p quarry-workbench --example bench_word_wrap --locked`
- **Viewport:** `40 visible rows × 12 columns` fetched via `Session::read_rows` from the midpoint of each file
- **Budgets:** Uniform `MAX_WRAPPED_LINES = 2`, `MAX_WRAP_CELL_BYTES = 4 KiB`, `MAX_WRAP_FRAME_BUDGET_BYTES = 128 KiB`, `MAX_CACHED_WRAP_ROWS = 512`, default `DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES = 300,000,000 B` (`300 MB`)

---

## 1. Wrap Cost vs. Total File Size (Real `Session::read_rows`)

| Target Size | Actual Size | Rows | Cold 40×12 Wrap Measurement | Warm `RowWrapCache` Lookup (40 rows) | Effective Wrap State (`enabled = true`) |
|---|---:|---:|---:|---:|---|
| `1 MB` | `1.00 MB` | `4,931` | `41.8 µs` *(cold icache/warmup)* | `2.12 µs` | `Active` |
| `50 MB` | `50.00 MB` | `246,549` | `15.1 µs` | `0.77 µs` | `Active` |
| `299 MB` | `299.00 MB` | `1,474,359` | `15.4 µs` | `0.76 µs` | `Active` |
| `301 MB` | `301.00 MB` | `1,484,221` | `15.1 µs` | `0.76 µs` | `AutoDisabled` (`> 300 MB`) |

### Finding 1 (File-Size Independence)
Because Quarry preserves O(1) row-based scrolling (`DataTable` / `uniform_list`, zero height prefix sums, zero full-file layout scans) and measures strictly fetched viewport rows inside a bounded 512-row `RowWrapCache`, wrap layout cost is **completely independent of total file size** (`15.1–15.4 µs` cold, `0.76–0.77 µs` warm across `50 MB` to `301 MB` / `1.48M` rows).

---

## 2. Hostile Single-Line Cell Budget Scaling (`40 × 12` Viewport)

| Cell Size | Per-Cell Wrap Measure | Full `40 × 12` Frame Wrap Cost | Bytes Inspected per Cell | Cells Degraded by 128 KiB Frame Cap |
|---|---:|---:|---:|---:|
| `64 B` | `0.07 µs` | `33.1 µs` | `39 B` | `0` |
| `4 KiB` | `1.23 µs` | `39.5 µs` | `39 B` | `448` |
| `64 KiB` | `1.28 µs` | `40.0 µs` | `39 B` | `448` |
| `1 MiB` | `1.24 µs` | `40.0 µs` | `39 B` | `448` |
| `10 MiB` | `1.24 µs` | `39.5 µs` | `39 B` | `448` |

### Finding 2 (Flat O(1) Worst-Case Frame Cost)
Even when every cell in a `40 × 12` viewport contains a `10 MiB` single-line string, `MAX_WRAP_CELL_BYTES` (`4 KiB`), early exit at `MAX_WRAPPED_LINES` (`2` lines), and `MAX_WRAP_FRAME_BUDGET_BYTES` (`128 KiB`) hold total frame wrap work flat at **`39.5–40.0 µs`** (`0.04 ms`, <0.5% of an `8.33 ms` 120 Hz frame budget).

---

## Architectural Decision (`C16` / `T15`)

- Keep the **300 MB** (`300,000,000 B`) default adaptive auto-disable setting (configurable in **File → Temporary Storage…** and persisted atomically) to preserve maximum single-line scan density on massive files by default while allowing one-click per-tab override.
- Enforce the 2-line cap, 4 KiB per-cell layout budget, and 128 KiB per-frame budget unconditionally whenever wrap is active (including manual overrides above 300 MB).
