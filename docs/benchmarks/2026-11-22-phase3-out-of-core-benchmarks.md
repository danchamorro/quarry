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

Measured on a deterministic 10,000,000-row synthetic CSV (`520,008,359 B`, ~495.9 MiB; generator and
full commands in Section 5) using the frozen `quarry-bench` binary with an isolated warm sidecar
cache. All three worker counts produced **100% byte-identical SHA-256 outputs** per operation.
`sort-save-as` stayed well beneath its printed ADR 0010 preflight allowance (`4.66 GiB`) at every
worker count; `duplicates --output` prints observed peak temporary disk (`3.83–28.30 MiB`, well
beneath input size) without printing its numeric preflight estimate; and `replace-all-save-as` uses
zero temporary spill runs (writing a single `520,008,359 B` output staging file beside the
destination):

| Operation | Workers (`W`) | Op Wall (s) | Cmd Wall (s) | Speedup vs `W=1` | Peak RSS | Measured Peak Temp / Preflight Allowance | Full Output SHA-256 |
|---|---:|---:|---:|---:|---:|---|---|
| `sort-save-as` (col 3, number) | 1 | 5.952 | 10.801 | 1.00× | 135.31 MiB | `1.31 GiB <= 4.66 GiB` (printed preflight) | `7f1def8658da6f4b8fb9a11335d7eb58df4033b7194295de9a494e0dc022bef9` |
| `sort-save-as` (col 3, number) | 4 | **2.983** | 7.643 | **2.00×** | 148.72 MiB | `1.31 GiB <= 4.66 GiB` (printed preflight) | Exact match |
| `sort-save-as` (col 3, number) | 8 | 3.867 | 8.328 | 1.54× | 165.69 MiB | `1.64 GiB <= 4.66 GiB` (printed preflight) | Exact match |
| `duplicates --output` (col 2, match-case) | 1 | 3.300 | 3.430 | 1.00× | 135.86 MiB | `3.83 MiB` peak temp (CLI preflight value not printed; formal numeric comparison pending) | `627f88eb7cfb487485ceb98db0f48c96b2fafad9bcb0c8227f46443db639c9e7` |
| `duplicates --output` (col 2, match-case) | 4 | 0.874 | 0.916 | 3.78× | 146.20 MiB | `14.13 MiB` peak temp (CLI preflight value not printed; formal numeric comparison pending) | Exact match |
| `duplicates --output` (col 2, match-case) | 8 | **0.491** | 0.521 | **6.72×** | 169.12 MiB | `28.30 MiB` peak temp (CLI preflight value not printed; formal numeric comparison pending) | Exact match |
| `replace-all-save-as` (`ST0042`→`ST9942`) | 1 | 0.785 | 0.915 | 1.00× | 5.23 MiB | `0 B` spill runs (`495.92 MiB` / `520,008,359 B` single destination staging file; no separate scratch allowance) | `be6d2332e51ceccaa91cc60faba61132b81e929e90adbdf811279ce480b81d2f` |
| `replace-all-save-as` (`ST0042`→`ST9942`) | 4 | **0.128** | 0.170 | **6.13×** | 105.64 MiB | `0 B` spill runs (`495.92 MiB` / `520,008,359 B` single destination staging file; no separate scratch allowance) | Exact match |
| `replace-all-save-as` (`ST0042`→`ST9942`) | 8 | 0.152 | 0.179 | 5.16× | 121.09 MiB | `0 B` spill runs (`495.92 MiB` / `520,008,359 B` single destination staging file; no separate scratch allowance) | Exact match |

---

## 3. Adaptive 300 MB Single-Run Memory Budget Transition (`T14` / `C16`)

Verified on deterministic wide-row synthetic CSVs (`16 KiB` rows so tracked payload closely tracks
raw file size; exact generator and commands in Section 5) using the frozen `quarry-bench` binary
across default `300,000,000 B` single-run budget vs explicit forced-spill `--memory-budget`:

### Delete Duplicates (`299,000,000 B` vs `301,000,000 B` under default 300 MB budget)

| Input Size | Configuration | In-Memory Fast Path | Runs Created | Op Wall (s) | Peak Temp | Peak RSS | Full Output SHA-256 |
|---|---|---|---:|---:|---:|---:|---|
| `299,000,000 B` (18,268 rows) | Default 300 MB (`--threads 1`) | **yes** | **0** | **0.593** | 234.13 MiB (output staging only) | 294.39 MiB | `e553a7fb258a704215b1200c8ae9194e22bd6aa820722c51a95ffd2052a5f8ff` |
| `299,000,000 B` (18,268 rows) | Forced spill `--memory-budget 64000000` (`--threads 4`) | no | 28 | 0.779 | 520.40 MiB | 114.23 MiB | **Exact match** |
| `301,000,000 B` (18,390 rows) | Default 300 MB (`--threads 1`) | **no** | **10** | 1.031 | 471.17 MiB | 326.48 MiB | **Exact match** |
| `301,000,000 B` (18,390 rows) | Forced spill `--memory-budget 64000000` (`--threads 4`) | no | 28 | 0.828 | 522.31 MiB | 115.27 MiB | **Exact match** |

### External Sort (`295,015,190 B` tracked payload `< 300 MB` vs `> 290 MB` & `301 MB`)

| Input Size | Configuration | In-Memory Fast Path | Runs Created | Op Wall (s) | Peak Temp | Peak RSS | Full Output SHA-256 |
|---|---|---|---:|---:|---:|---:|---|
| `295,015,190 B` | Default 300 MB (`--threads 1`) | **yes** | **0** | **0.534** | 281.35 MiB (output staging only) | 290.56 MiB | `32d49d2cbc3c137a0b10c58e231bfbf83369bbf87586966b41931f365d5b88b9` |
| `295,015,190 B` | `--memory-budget 290000000` (`--threads 1`) | **no** | **2** | 0.762 | 563.35 MiB | 290.59 MiB | **Exact match** |
| `295,015,190 B` | Default 300 MB (`--threads 4`, 4-worker budget split) | no | 8 | 0.528 | 563.35 MiB | 308.91 MiB | **Exact match** |
| `295,015,190 B` | Forced spill `--memory-budget 64000000` (`--threads 4`) | no | 20 | 0.525 | 563.35 MiB | 85.55 MiB | **Exact match** |
| `301,000,000 B` | Default 300 MB (`--threads 1`) | **no** | **2** | 0.772 | 574.78 MiB | 294.39 MiB | `4de72cdffcfe8a68b37e9272f3935c3eb1331e9016ba4d0d62b3c6ef7d23ff24` (matches 64 MB forced spill) |

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

### 5.1 Full 30-Workload Public Dataset Suite & Golden SHA-256 Parity

```bash
CACHE_DIR="$(mktemp -d)" && chmod 700 "$CACHE_DIR"
PYTHONDONTWRITEBYTECODE=1 QUARRY_INDEX_CACHE_DIR="$CACHE_DIR" python3 scripts/benchmark-public-datasets.py . \
  /tmp/quarry-phase3-public-bench \
  --expected-outputs docs/benchmarks/2026-11-21-phase1-public-datasets-golden.json \
  --threads 8
```

### 5.2 Clean-Main Join Acceptance Suite (`narrow`, `large`, `skew`)

```bash
PYTHONDONTWRITEBYTECODE=1 python3 scripts/bench-join-acceptance.py --case narrow --root /tmp/quarry-phase3-join-narrow
PYTHONDONTWRITEBYTECODE=1 python3 scripts/bench-join-acceptance.py --case large --root /tmp/quarry-phase3-join-large --allow-below-ram
PYTHONDONTWRITEBYTECODE=1 python3 scripts/bench-join-acceptance.py --case skew --root /tmp/quarry-phase3-join-skew
```

### 5.3 Section 2: `W ∈ {1, 4, 8}` Scaling Synthetic Generator & Commands

Generate the deterministic 10,000,000-row CSV (`520,008,359 B`, no external seed needed) and run
each operation at `W ∈ {1, 4, 8}` against the frozen release binary `BENCH`:

```bash
BENCH=/tmp/quarry-phase3-public-bench/quarry-bench
python3 - <<'PY'
from pathlib import Path
p = Path("/tmp/synth_500mb.csv")
with p.open("wb") as f:
    f.write(b"id,station,temp,flag,note\n")
    buf = bytearray()
    for i in range(1, 10_000_001):
        st = f"ST{(i * 31) % 5000:04d}"
        temp = ((i * 17) % 1200 - 200) / 10.0
        buf.extend(f"{i:08d},{st},{temp:.1f},OK,record-payload-{i:08d}-end\n".encode())
        if len(buf) >= 4 * 1024 * 1024:
            f.write(buf); buf.clear()
    if buf:
        f.write(buf)
PY

# Seed warm sidecar index once:
QUARRY_INDEX_CACHE_DIR="$CACHE_DIR" "$BENCH" open /tmp/synth_500mb.csv --rows 10 --metrics-only --threads 8 --cache-state warm

# Repeat for W in 1 4 8:
for W in 1 4 8; do
  mkdir -p "/tmp/scale-sort-w$W" "/tmp/scale-dup-w$W" "/tmp/scale-rep-w$W"
  QUARRY_INDEX_CACHE_DIR="$CACHE_DIR" "$BENCH" sort-save-as /tmp/synth_500mb.csv "/tmp/scale-sort-w$W/out.csv" \
    --column 3 --mode number --order asc --header first-row --temp-dir "/tmp/scale-sort-w$W" --threads "$W" --cache-state warm
  QUARRY_INDEX_CACHE_DIR="$CACHE_DIR" "$BENCH" duplicates /tmp/synth_500mb.csv \
    --columns 2 --match-case --header first-row --temp-dir "/tmp/scale-dup-w$W" --output "/tmp/scale-dup-w$W/out.csv" --threads "$W" --cache-state warm
  QUARRY_INDEX_CACHE_DIR="$CACHE_DIR" "$BENCH" replace-all-save-as /tmp/synth_500mb.csv \
    --output "/tmp/scale-rep-w$W/out.csv" --query ST0042 --replacement ST9942 --threads "$W" --cache-state warm
done
```

### 5.4 Section 3: Adaptive 300 MB Single-Run Threshold Generator & Commands

Generate the deterministic wide-row CSVs (`295,015,190 B`, `299,000,000 B`, and `301,000,000 B`)
and compare the default `300,000,000 B` single-run budget against explicit forced-spill budgets:

```bash
python3 - <<'PY'
from pathlib import Path

def make_wide_csv(path: Path, target_bytes: int, exact_tail: bool):
    header = b"id,key,payload\n"
    pad = "P" * 16350
    with path.open("wb") as f:
        f.write(header)
        written, i, buf = len(header), 0, bytearray()
        while written < target_bytes:
            k = (i * 7919) % 15000
            line = f"{i:08d},{k:06d},{pad}\n".encode()
            if exact_tail and written + len(line) > target_bytes and written > target_bytes - 20000:
                rem = target_bytes - written
                prefix = f"{i:08d},{k:06d},".encode()
                if rem > len(prefix) + 1:
                    buf.extend(prefix + (b"P" * (rem - len(prefix) - 1)) + b"\n")
                break
            buf.extend(line); written += len(line); i += 1
            if len(buf) >= 4 * 1024 * 1024:
                f.write(buf); buf.clear()
        if buf:
            f.write(buf)

make_wide_csv(Path("/tmp/file_295mb.csv"), 295_000_000, exact_tail=False) # 295,015,190 B
make_wide_csv(Path("/tmp/file_299mb.csv"), 299_000_000, exact_tail=True)  # 299,000,000 B
make_wide_csv(Path("/tmp/file_301mb.csv"), 301_000_000, exact_tail=True)  # 301,000,000 B
PY

# Delete Duplicates: 299 MB (in-memory fast path) vs 301 MB (spill) vs forced 64 MB spill
"$BENCH" duplicates /tmp/file_299mb.csv --columns 2 --match-case --header first-row --temp-dir /tmp/t299-d1 --output /tmp/t299-d1/out.csv --threads 1 --cache-state warm
"$BENCH" duplicates /tmp/file_299mb.csv --columns 2 --match-case --header first-row --temp-dir /tmp/t299-d4s --output /tmp/t299-d4s/out.csv --threads 4 --memory-budget 64000000 --cache-state warm
"$BENCH" duplicates /tmp/file_301mb.csv --columns 2 --match-case --header first-row --temp-dir /tmp/t301-d1 --output /tmp/t301-d1/out.csv --threads 1 --cache-state warm
"$BENCH" duplicates /tmp/file_301mb.csv --columns 2 --match-case --header first-row --temp-dir /tmp/t301-d4s --output /tmp/t301-d4s/out.csv --threads 4 --memory-budget 64000000 --cache-state warm

# Sort: 295 MB (in-memory fast path) vs 290 MB budget spill vs 4-worker split vs 64 MB forced spill vs 301 MB spill
"$BENCH" sort-save-as /tmp/file_295mb.csv /tmp/t295-s1/out.csv --column 2 --mode number --order asc --header first-row --temp-dir /tmp/t295-s1 --threads 1 --cache-state warm
"$BENCH" sort-save-as /tmp/file_295mb.csv /tmp/t295-s290/out.csv --column 2 --mode number --order asc --header first-row --temp-dir /tmp/t295-s290 --threads 1 --memory-budget 290000000 --cache-state warm
"$BENCH" sort-save-as /tmp/file_295mb.csv /tmp/t295-s4/out.csv --column 2 --mode number --order asc --header first-row --temp-dir /tmp/t295-s4 --threads 4 --cache-state warm
"$BENCH" sort-save-as /tmp/file_295mb.csv /tmp/t295-s64/out.csv --column 2 --mode number --order asc --header first-row --temp-dir /tmp/t295-s64 --threads 4 --memory-budget 64000000 --cache-state warm
"$BENCH" sort-save-as /tmp/file_301mb.csv /tmp/t301-s1/out.csv --column 2 --mode number --order asc --header first-row --temp-dir /tmp/t301-s1 --threads 1 --cache-state warm
```
