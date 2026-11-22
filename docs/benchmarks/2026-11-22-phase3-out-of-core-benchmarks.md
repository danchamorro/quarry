# Phase 3 Parallel Out-of-Core, Adaptive Budget & Hash-Join Benchmarks: 2026-11-22

This report records the Phase 3.4 (`#172`) verification benchmarks and golden output digest parity
checks on clean merged `main` (`f1ae14917f69e885f5608ea539b6a1dea796af0d`), covering:

- Checkpoint-striped parallel external Sort, adaptive 300 MB single-run memory budget, and persisted
  scratch directory (`#173` / `#167`, ADR 0005 amendment),
- Parallel Delete Duplicates and Replace All with completed output indexes (`#174` / `#168`),
- Natural Sort (`SortMode::Natural`), whole-row Duplicates (`DuplicateTarget::AllFields`), and
  bounded date-format suggestion (`#175` / `#169`, ADR 0004 & ADR 0010 amendments),
- Compact key-only frames + late materialization (`#176` / `#170`) and partitioned multi-threaded
  hash-join with automatic ADR 0011 sort-merge skew fallback (`#177` / `#171`, ADR 0011 amendment).

All public-dataset comparisons run on the same host-Mac bare-metal venue against the frozen Phase 1
Gate 1A baseline ([2026-11-21-phase1-public-datasets-baseline.md](2026-11-21-phase1-public-datasets-baseline.md)),
machine-readable golden digests ([2026-11-21-phase1-public-datasets-golden.json](2026-11-21-phase1-public-datasets-golden.json)),
and Phase 2 engine report ([2026-11-21-phase2-engine-benchmarks.md](2026-11-21-phase2-engine-benchmarks.md)).

## Environment & Provenance

| Item | Value |
|---|---|
| Venue | `host-bare-metal` (headless CLI only; desktop `.app` remains VM-only) |
| Machine | MacBook Pro, Apple M3 Max, 16 cores, 128 GiB RAM |
| Operating system | macOS 27.0.1 (`arm64`) |
| Engine revision | `f1ae14917f69e885f5608ea539b6a1dea796af0d` (`clean`) |
| Frozen `quarry-bench` SHA-256 | `3360001d535766c73f09ee6e3d346c315449ad2fbd09bb4e067f98f27ef80114` |
| Frozen `join_acceptance` SHA-256 | `c8025b17448265192dc5730dd35694249b92096ffaa6896b334f6ed1663f3edf` |
| Worker configuration | `--threads 8` (plus `W ∈ {1, 4, 8}` scaling sweep) |
| Cache protocol | `warm` — Full SHA-256 read before suite start; isolated `0700` `QUARRY_INDEX_CACHE_DIR` |
| 30-Workload Suite UTC | `2026-11-22T19:54:58.334375+00:00` -> `2026-11-22T20:19:05.179473+00:00` (24 min 07 s total vs 50 min 30 s in Phase 2) |
| Sources unchanged | `True` (exact SHA-256 match before and after) |
| Golden output parity | **14/14 output-producing workloads** matched Phase 1 SHA-256, bytes, and quote-aware row counts (`30/30` workloads exited `0`; `0` unterminated quotes) |
| Max child RSS (30-workload suite) | **194.25 MiB** (`203,685,888 B`, well within the `<= 256 MiB` bounded ceiling) |

---

## 1. Full 30-Workload Matched-Venue Comparison (Phase 1 vs Phase 2 vs Phase 3, `--threads 8`)

Executed via:
`QUARRY_INDEX_CACHE_DIR=/tmp/quarry-phase3-cache python3 scripts/benchmark-public-datasets.py . /tmp/quarry-phase3-public-bench --expected-outputs docs/benchmarks/2026-11-21-phase1-public-datasets-golden.json --threads 8`

### Phase 3 Target Operations Highlight (Operation Wall Time vs Whole Command Time)

| Dataset | Operation | Phase 1 Cmd (s) | Phase 2 Cmd (s) | Phase 3 Cmd (s) | Phase 3 Op Wall (s) | Speedup vs Phase 1 | Peak RSS | Peak Temp vs Preflight Est. | Golden Match |
|---|---|---:|---:|---:|---:|---:|---:|---|---|
| `weather` | `replace-all` | 187.803 | 153.244 | **3.121** | **2.800** (`3.25 GiB/s`) | **60.2×** | 178.91 MiB | 9.10 GiB staging | Exact (`99df9f38…`) |
| `hacker-news` | `replace-all` | 44.833 | 38.583 | **3.207** | **2.760** (`4.29 GiB/s`) | **14.0×** | 193.19 MiB | 11.84 GiB staging | Exact (`d4e78f74…`) |
| `weather` | `sort-number` | 805.252 | 223.719 | **146.046**\* | **50.968** (`0.398 ms` adopt) | **5.51×** | 172.05 MiB | 22.00 GiB `<=` 83.12 GiB | Exact (`4be7c669…`) |
| `hacker-news` | `sort-number` | 170.205 | 128.599 | **90.254**\* | **38.974** (`0.496 ms` adopt) | **1.89×** | 164.47 MiB | 24.40 GiB `<=` 97.00 GiB | Exact (`b7462bd3…`) |
| `weather` | `duplicates` | 271.080 | 673.355 | **123.754** | 123.754 | **2.19×** | 167.88 MiB | 30.11 GiB | — |
| `weather` | `duplicates-remove` | 264.782 | 262.692 | **121.441** | **120.677** | **2.18×** | 174.25 MiB | 30.12 GiB | Exact (`10e09e36…`) |
| `hacker-news` | `duplicates` | 148.002 | 112.048 | **76.620** | 76.620 | **1.93×** | 193.03 MiB | 25.58 GiB | — |
| `hacker-news` | `duplicates-remove` | 353.288 | 108.958 | **75.822** | **74.870** | **4.66×** | 194.25 MiB | 25.61 GiB | Exact (`165d9336…`) |

*\*Note: `quarry-bench sort-save-as` performs an independent post-sort full-file re-index and dual-fingerprint validation pass inside the command timing (`72.757 s` on Weather and `21.233 s` on Hacker News). The actual parallel 8-worker sort wall times were **50.968 s** (Weather, 9.10 GiB / 131.99M rows) and **38.974 s** (Hacker News, 11.84 GiB / 28.74M rows), and completed output index adoption took **0.398 ms** and **0.496 ms** respectively.*

### Complete 30-Workload Table

| Dataset | Case | Phase 1 (s) | Phase 2 (s) | Phase 3 (s) | Phase 3 vs Phase 1 | Phase 3 Peak RSS | Peak Scratch | Golden Match |
|---|---|---:|---:|---:|---:|---:|---:|---|
| `weather` | `open` | 37.936 | 0.458 | **0.599** | 63.3× | 24.5 MiB | 0 | — |
| `weather` | `viewport` | 45.543 | 0.303 | **0.303** | 150.3× | 6.5 MiB | 0 | — |
| `weather` | `find-early` | 38.791 | 0.013 | **0.013** | 2,983.9× | 22.4 MiB | 0 | — |
| `weather` | `find-absent` | 110.366 | 0.338 | **0.317** | 348.2× | 41.0 MiB | 0 | — |
| `weather` | `filter-numeric` | 70.412 | 1.058 | **1.060** | 66.4× | 45.6 MiB | 0 | — |
| `weather` | `filter-and` | 68.644 | 1.069 | **1.068** | 64.3× | 24.6 MiB | 0 | — |
| `weather` | `export` | 69.330 | 1.155 | **1.140** | 60.8× | 24.5 MiB | 2.1 MiB | Exact (`4cc76824…`, 26,506 rows) |
| `weather` | `edit-save-as` | 84.779 | 9.315 | **8.929** | 9.5× | 25.6 MiB | 9.10 GiB | Exact (`1dda7240…`, 131,985,330 rows) |
| `weather` | `replace-all` | 187.803 | 153.244 | **3.121** | **60.2×** | 178.9 MiB | 9.10 GiB | Exact (`99df9f38…`, 131,985,330 rows) |
| `weather` | `split-auto` | 335.654 | 355.010 | **162.863** | 2.1× (serial) | 30.4 MiB | 9.10 GiB | Exact (`a3473c04…`, 131,985,330 rows) |
| `weather` | `combine` | 271.298 | 441.560 | **136.741** | 2.0× (serial) | 30.7 MiB | 9.10 GiB | Exact (`c5f57f5d…`, 131,985,330 rows) |
| `weather` | `sort-number` | 805.252 | 223.719 | **146.046** | **5.51×** | 172.0 MiB | 22.00 GiB | Exact (`4be7c669…`, 131,985,330 rows) |
| `weather` | `duplicates` | 271.080 | 673.355 | **123.754** | **2.19×** | 167.9 MiB | 30.11 GiB | — |
| `weather` | `duplicates-remove` | 264.782 | 262.692 | **121.441** | **2.18×** | 174.2 MiB | 30.12 GiB | Exact (`10e09e36…`, 127,261,910 rows) |
| `weather` | `sort-cancel` | 45.611 | 15.185 | **12.043** | 3.8× | 108.0 MiB | 0 | — (0 leaked files) |
| `hacker-news` | `open` | 7.812 | 4.244 | **3.587** | 2.2× | 15.5 MiB | 0 | — |
| `hacker-news` | `viewport` | 8.815 | 0.421 | **0.406** | 21.7× | 8.3 MiB | 0 | — |
| `hacker-news` | `find-early` | 7.226 | 0.015 | **0.015** | 481.7× | 22.5 MiB | 0 | — |
| `hacker-news` | `find-absent` | 22.118 | 0.511 | **0.514** | 43.0× | 39.3 MiB | 0 | — |
| `hacker-news` | `filter-numeric` | 14.360 | 1.640 | **1.693** | 8.5× | 32.3 MiB | 0 | — |
| `hacker-news` | `filter-and` | 15.221 | 1.573 | **1.600** | 9.5× | 33.9 MiB | 0 | — |
| `hacker-news` | `export` | 14.544 | 1.657 | **1.499** | 9.7× | 28.8 MiB | 51.6 MiB | Exact (`1ebb1468…`, 128,223 rows) |
| `hacker-news` | `edit-save-as` | 21.492 | 10.497 | **10.798** | 2.0× | 29.3 MiB | 11.84 GiB | Exact (`e8495618…`, 28,737,558 rows) |
| `hacker-news` | `replace-all` | 44.833 | 38.583 | **3.207** | **14.0×** | 193.2 MiB | 11.84 GiB | Exact (`d4e78f74…`, 28,737,558 rows) |
| `hacker-news` | `split-auto` | 79.997 | 56.159 | **53.079** | 1.5× (serial) | 28.0 MiB | 11.46 GiB | Exact (`47e1d395…`, 28,737,558 rows) |
| `hacker-news` | `combine` | 67.062 | 45.476 | **42.699** | 1.6× (serial) | 29.2 MiB | 11.46 GiB | Exact (`62555d7e…`, 28,737,558 rows) |
| `hacker-news` | `sort-number` | 170.205 | 128.599 | **90.254** | **1.89×** | 164.5 MiB | 24.38 GiB | Exact (`b7462bd3…`, 28,737,558 rows) |
| `hacker-news` | `duplicates` | 148.002 | 112.048 | **76.620** | **1.93×** | 193.0 MiB | 25.58 GiB | — |
| `hacker-news` | `duplicates-remove` | 353.288 | 108.958 | **75.822** | **4.66×** | 194.2 MiB | 25.59 GiB | Exact (`165d9336…`, 28,737,558 rows) |
| `hacker-news` | `sort-cancel` | 23.652 | 15.973 | **15.065** | 1.6× | 83.1 MiB | 0 | — (0 leaked files) |

---

## 2. Parallel Scaling & Disk Preflight Verification (`W ∈ {1, 4, 8}`)

Measured on a deterministic 10,000,000-row synthetic CSV (`520,008,359 B`, ~495.9 MiB) using the
frozen `quarry-bench` binary with an isolated warm sidecar cache. All three worker counts produced
**100% byte-identical SHA-256 outputs** per operation and kept observed peak temporary disk well
beneath the advertised preflight allowance:

| Operation | Workers (`W`) | Op Wall (s) | Cmd Wall (s) | Speedup vs `W=1` | Peak RSS | Peak Temp vs Preflight Est. | SHA-256 Parity |
|---|---:|---:|---:|---:|---:|---|---|
| `sort-save-as` (number) | 1 | 5.952 | 10.801 | 1.00× | 135.31 MiB | 1.31 GiB `<=` 4.66 GiB | `7f1def86…` |
| `sort-save-as` (number) | 4 | **2.983** | 7.643 | **2.00×** | 148.72 MiB | 1.31 GiB `<=` 4.66 GiB | Exact match |
| `sort-save-as` (number) | 8 | 3.867 | 8.328 | 1.54× | 165.69 MiB | 1.64 GiB `<=` 4.66 GiB | Exact match |
| `duplicates --output` | 1 | 3.300 | 3.430 | 1.00× | 135.86 MiB | 3.83 MiB | `627f88eb…` |
| `duplicates --output` | 4 | 0.874 | 0.916 | 3.78× | 146.20 MiB | 14.13 MiB | Exact match |
| `duplicates --output` | 8 | **0.491** | 0.521 | **6.72×** | 169.12 MiB | 28.30 MiB | Exact match |
| `replace-all-save-as` | 1 | 0.785 | 0.915 | 1.00× | 5.23 MiB | Staging only | `be6d2332…` |
| `replace-all-save-as` | 4 | **0.128** | 0.170 | **6.13×** | 105.64 MiB | Staging only | Exact match |
| `replace-all-save-as` | 8 | 0.152 | 0.179 | 5.16× | 121.09 MiB | Staging only | Exact match |

---

## 3. Adaptive 300 MB Single-Run Memory Budget Transition (`T14` / `C16`)

Verified on deterministic wide-row synthetic CSVs (`16 KiB` rows so tracked payload closely tracks
raw file size) using the frozen `quarry-bench` binary across default `300,000,000 B` single-run
budget vs explicit forced-spill `--memory-budget`:

### Delete Duplicates (`299,000,000 B` vs `301,000,000 B` under default 300 MB budget)

| Input Size | Configuration | In-Memory Fast Path | Runs Created | Op Wall (s) | Peak Temp | Peak RSS | Output SHA-256 |
|---|---|---|---:|---:|---:|---:|---|
| `299,000,000 B` (18,268 rows) | Default 300 MB (`--threads 1`) | **yes** | **0** | **0.593** | 234.13 MiB (output staging only) | 294.39 MiB | `e553a7fb…` |
| `299,000,000 B` (18,268 rows) | Forced spill `--memory-budget 64000000` (`--threads 4`) | no | 28 | 0.779 | 520.40 MiB | 114.23 MiB | **Exact match** (`e553a7fb…`) |
| `301,000,000 B` (18,390 rows) | Default 300 MB (`--threads 1`) | **no** | **10** | 1.031 | 471.17 MiB | 326.48 MiB | **Exact match** (`e553a7fb…`) |
| `301,000,000 B` (18,390 rows) | Forced spill `--memory-budget 64000000` (`--threads 4`) | no | 28 | 0.828 | 522.31 MiB | 115.27 MiB | **Exact match** (`e553a7fb…`) |

### External Sort (`295,015,190 B` tracked payload `< 300 MB` vs `> 290 MB` & `301 MB`)

| Input Size | Configuration | In-Memory Fast Path | Runs Created | Op Wall (s) | Peak Temp | Peak RSS | Output SHA-256 |
|---|---|---|---:|---:|---:|---:|---|
| `295,015,190 B` | Default 300 MB (`--threads 1`) | **yes** | **0** | **0.534** | 281.35 MiB (output staging only) | 290.56 MiB | `32d49d2c…` |
| `295,015,190 B` | `--memory-budget 290000000` (`--threads 1`) | **no** | **2** | 0.762 | 563.35 MiB | 290.59 MiB | **Exact match** (`32d49d2c…`) |
| `295,015,190 B` | Default 300 MB (`--threads 4`, 4-worker budget split) | no | 8 | 0.528 | 563.35 MiB | 308.91 MiB | **Exact match** (`32d49d2c…`) |
| `295,015,190 B` | Forced spill `--memory-budget 64000000` (`--threads 4`) | no | 20 | 0.525 | 563.35 MiB | 85.55 MiB | **Exact match** (`32d49d2c…`) |
| `301,000,000 B` | Default 300 MB (`--threads 1`) | **no** | **2** | 0.772 | 574.78 MiB | 294.39 MiB | `4de72cdf…` (matches 64 MB forced spill) |

Because `sort-save-as` tracks both raw record bytes and encoded sort key + entry metadata in its
unified `RunArena`, a `295.0 MB` wide-row file stays in-memory with `0` spill runs under the default
`300,000,000 B` budget (`--threads 1`), while lowering the budget to `290,000,000 B` or growing the
input to `301,000,000 B` spills seamlessly into identical sorted bytes. With `--threads 4`, the
shared `300 MB` budget is divided across active worker arenas, keeping peak RSS bounded while
producing 100% byte-identical output.

---

## 4. Partitioned Hash-Join & Skew Fallback Verification (`f1ae149`, `T6` / `C24`)

Re-verified on clean merged `main` (`f1ae149`, including all post-review FD budgeting, bounded
key-digest previews/samples, retained-table checks, and fallback memory-budgeting fixes) via
`scripts/bench-join-acceptance.py`:

| Workload | Baseline (`6bc42a9`) Total | Phase 3 (`f1ae149`) Analysis | Phase 3 (`f1ae149`) Creating | Phase 3 (`f1ae149`) Total | Speedup vs Baseline | Peak RSS (`time -l`) | Peak Temp vs Allowance | Oracle Parity |
|---|---:|---:|---:|---:|---:|---:|---|---|
| `narrow` (10M rows/side, Left Join) | 422.57 s | **5.402 s** | **7.067 s** | **12.494 s** | **33.8×** | **490.52 MiB** (`< 512 MiB`) | `1.65 GB <= 1.76 GB` | `all_checks_passed: true` |
| `large` (18.0 GiB, 73,728 × 128 KiB/side, Left Join) | 96.74 s | **7.843 s** | **11.534 s** | **19.401 s** | **4.99×** | **16.30 MiB** | `9.68 GB <= 9.69 GB` | `all_checks_passed: true` |
| `skew` (1,024 × 1,536 = 1,572,864 Cartesian pairs, 814.7 MB output) | — | **0.011 s** | **2.767 s** | **2.800 s** | Automatic skew fallback | **8.28 MiB** (`32,768 B <= 131,072 B` group buffer) | `927.99 MB <= 1.04 GB` | `all_checks_passed: true` |

*(See [2026-11-22-phase3c-join-profiling.md](2026-11-22-phase3c-join-profiling.md) for the full
Phase 3C-1/3C-2/3C-3 phase breakdown.)*

---

## 5. Reproduction Commands

```bash
# 1. Full 30-workload public dataset suite & Phase 1 golden SHA-256 verification:
CACHE_DIR="$(mktemp -d)" && chmod 700 "$CACHE_DIR"
PYTHONDONTWRITEBYTECODE=1 QUARRY_INDEX_CACHE_DIR="$CACHE_DIR" python3 scripts/benchmark-public-datasets.py . \
  /tmp/quarry-phase3-public-bench \
  --expected-outputs docs/benchmarks/2026-11-21-phase1-public-datasets-golden.json \
  --threads 8

# 2. Clean-main Join acceptance suite (narrow, large, skew):
PYTHONDONTWRITEBYTECODE=1 python3 scripts/bench-join-acceptance.py --case narrow --root /tmp/quarry-phase3-join-narrow
PYTHONDONTWRITEBYTECODE=1 python3 scripts/bench-join-acceptance.py --case large --root /tmp/quarry-phase3-join-large --allow-below-ram
PYTHONDONTWRITEBYTECODE=1 python3 scripts/bench-join-acceptance.py --case skew --root /tmp/quarry-phase3-join-skew
```
