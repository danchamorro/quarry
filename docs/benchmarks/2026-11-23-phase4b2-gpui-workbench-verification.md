# Phase 4B-2 GPUI Multi-Window, 200-Tab Workbench & Theme Verification

**Date:** `2026-11-23`  
**Scope:** Phase 4B-2 (`#193` / PR `#197`, `#194` / PR `#198`, `#195` / PR `#199`, and `#196`)  
**Default Packaging Status:** `apps/quarry-egui` remains intact and default in `scripts/macos-app.sh` throughout Phase 4B-2; single packaging cutover to `apps/quarry-gpui` is gated on Phase 4B-3 owner sign-off.

---

## 1. Automated Verification Matrix

Executed with `--locked` on both VMPal guest environments at commit `6db1343`:

| Venue | OS / Architecture | Command | Result |
|---|---|---|---|
| `macOS VM 1` | macOS `27.2.1` (build `26B131`, `aarch64`) | `cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test -p quarry-workbench -p quarry-gpui --locked` | **PASS** (`0` warnings; `112/112` tests: `17` `quarry-gpui` UI/panel tests, `65` `quarry-workbench` unit tests, `2` `command_slice`, `28` `dormant_tabs`) |
| `Omarchy VM 4` | Omarchy `7.3.0-3.1-omarchy` (`aarch64`, Hyprland `0.56.2`) | `cargo clippy -p quarry-workbench -p quarry-gpui --all-targets --locked -- -D warnings && cargo test -p quarry-workbench -p quarry-gpui --locked` | **PASS** (`0` warnings; `112/112` tests: `17` `quarry-gpui` UI/panel tests, `65` `quarry-workbench` unit tests, `2` `command_slice`, `28` `dormant_tabs`) |

Key Phase 4B-2 automated test coverage added in `apps/quarry-gpui/src/ui_tests.rs` and `crates/quarry-workbench/`:

- **Multi-window & 200-tab dormancy (`#193`, `#196`):** `two_hundred_tabs_across_four_windows_stay_bounded_and_preserve_dirty_work` opens 200 synthetic CSV files across 4 GPUI windows (`50` tabs/window), confirms `>= 1` hydrated tab per window, keeps internal FD reservations `< 128`, edits a tab in Window 1, cycles through 11 sibling tabs to force dormancy eviction, and verifies the dirty cell overlay survives wake intact.
- **Full interactive feature parity (`#194`, `#195`):** Find/Replace/Replace All, multi-rule & numeric Filters + Filtered Export, Multi-column Sort (Text/Number/DateTime/Natural), Duplicates preview & removal, Cleanup, Insert/Delete/Move/Split/Combine Columns, Combine Files, 4-mode Join Files (`Left`/`Inner`/`Right`/`Full Outer`) with live `DocumentLease`s, `.gz` streaming lifecycle (Preview / Decompress / Cancel / Save As uncompressed), Inspect File & Recovery (active tab, failed-open single & multi-file batch, arbitrary path, and raw `.gz`), Temporary Storage & Sidecar Index Cache settings, and About Quarry.
- **Themes & typography (`#196`):** WCAG AA (`>= 4.5:1`) contrast unit tests for `LIGHT`, `DARK`, and Omarchy `tokyo-night`; atomic private `0600` persistence for `ThemePreferences`; hardened Omarchy allowlist (`{ ~/.config/omarchy, ~/.local/share/omarchy, ~/.local/state/omarchy }`) symlink-escape rejection, `64 KiB` cap, low-contrast fallback, and live `theme.generation` reload; and OpenType `tnum` (`FontFeatures([("tnum", 1)])`) tabular numeric formatting with right-aligned `source_looks_numeric` cells.

---

## 2. Live 200-File Corpus & Multi-Window VM Measurements

Synthetic corpus generated deterministically in each VM using:

```bash
python3 scripts/generate-200-file-corpus.py --output-dir /tmp/quarry-200-corpus
```

- **Corpus manifest:** `200` files (`194` plain CSVs + `6` `.csv.gz` files, `5,062,590` bytes on disk, `400–500` rows per file).

| Venue & Scenario | Processes | Resident Memory (RSS) | Open FDs | Windowing / Accessibility |
|---|---:|---:|---:|---|
| `macOS VM 1` — Light theme, 1 window, 200 open tabs | `1` | `141.39 MiB` (`144,784 KiB`) | `8` numeric (`4` regular; `44` `lsof` total) | Native macOS Metal GPUI window; right-aligned `tnum` SF Pro digits; status bar `Index complete · 200 tabs (1 active)` |
| `macOS VM 1` — Dark theme, 1 window, 200 open tabs | `1` | `148.62 MiB` (`152,192 KiB`) | `8` numeric (`4` regular) | Native macOS Dark palette (`#131417` canvas, `#1A1B1E` chrome, `#E9C46A` amber accent, `#EFCE7A` row numbers); native macOS menu bar (`QuarryGPUI · File · Edit · Data · View · Window`) |
| `macOS VM 1` — Multi-window (`Move to Window`), 2 windows, 200 open tabs, Omarchy `tokyo-night` palette | `1` | `153.02 MiB` (`156,688 KiB`) | `8` numeric (`4` regular) | Live `accesskit_macos` tree verified via `vmpal_tree`: 2 native windows (`[a13]`, `[a17]`), `tab group "Open documents"`, `toolbar "Document toolbar"`, `table "table_000.csv data grid"` with column headers and per-cell `column: value` labels |
| `Omarchy VM 4` — Live `~/.local/state/omarchy/current/theme/colors.toml` (`tokyo-night`), 1 window, 200 open tabs | `1` | `320.29 MiB` `VmRSS` / `351.13 MiB` `VmHWM` | `35` kernel FDs (`/proc/<pid>/fd`) | `hyprctl clients -j`: `"class": "dev.quarry.gpui"`, `"title": "table_000.csv · Quarry"`, `"xwayland": false` (100% native Wayland) |

### Honest Venue Boundaries

- All 200-tab measurements stay well below the Phase 4A/4B gate (`<= 1 GiB` peak RSS and `< 128` process FDs) in a single process on both macOS and Linux/Omarchy.
- On `Omarchy VM 4`, Hyprland's session was screen-locked (`solitaryBlockedBy: ["LOCK", ...]`) during this run, so unlocked visual pixel inspection of `1.25×`/`1.5×` fractional scaling and unlocked AT-SPI inspection remain tracked for an unlocked Linux/Omarchy session in Phase 4B-3 / 9B (building on the unlocked `accesskit_unix 0.22.1` AT-SPI baseline recorded in `docs/benchmarks/2026-11-23-phase4b1-gpui-hardening.md`).
- 120 Hz hardware frame-deadline tracing (`C07`/`C08`) remains scoped to an owner-approved physical display venue (`T18`) in Phase 4B-3 / Phase 6.

---

## 3. WCAG AA Contrast & Security Audit Summary

- **Built-in palettes (`crates/quarry-workbench/src/theme.rs`):** Both `LIGHT` and `DARK` satisfy `palette_meets_wcag_aa` (`>= 4.5:1` across all semantic text/background pairs, `>= 10.0:1` primary text on canvas/zebra/chrome, `>= 7.0:1` `on_accent` over `ACCENT`).
- **Omarchy local theme sync (`load_omarchy_theme_from_home`):**
  - Strictly restricted to `{ ~/.config/omarchy, ~/.local/share/omarchy, ~/.local/state/omarchy }`.
  - Canonicalizes both allowed roots and candidate files; symlinks resolving inside the 3-root allowlist are accepted while symlink escapes outside the allowlist are rejected cleanly with a warning and safe fallback.
  - Enforces regular-file check, `64 KiB` (`MAX_OMARCHY_THEME_BYTES`) read cap, and WCAG AA contrast validation (`palette_meets_wcag_aa`), falling back to `LIGHT`/`DARK` if malformed or low-contrast.
  - Zero new external crates added (`Cargo.lock` unchanged).
