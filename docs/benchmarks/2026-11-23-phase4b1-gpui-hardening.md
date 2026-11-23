# Phase 4B-1: GPUI Technical Hardening, Zero-Transport Proof, and Live Verification

**Date:** 2026-11-23
**Commits verified:** `8d16425` (PR `#191`, workspace/toolchain/Zero-HTTP unification) and `40ff4d2` (branch `feat/phase4b1-gpui-verification-and-adr`, quirk fixes + accessibility + live VM verification)
**Related:** [ADR 0013](../adr/0013-adopt-gpui-desktop-frontend.md), [apps/quarry-gpui/VALIDATION.md](../../apps/quarry-gpui/VALIDATION.md), [docs/V1_CLAIM_LEDGER.md](../V1_CLAIM_LEDGER.md), issues `#189` and `#190`

## Summary

Phase 4B-1 hardened `apps/quarry-gpui` inside the root Cargo workspace before committing to full v1.0 feature parity work in Phase 4B-2:

1. Unified `apps/quarry-gpui` into the root Cargo workspace and single `Cargo.lock` on `gpui-kit = "=0.10.0"` and Rust `1.100.0` (PR `#191`).
2. Proved Zero-HTTP / Zero-Transport across all four shipping target graphs and linked macOS/Linux release binaries.
3. Fixed both `apps/quarry-gpui/VALIDATION.md` prototype quirks (horizontal scrolling + nested popover `Select` outside-click switching) and wired per-cell grid accessibility labels, semantic roles, and dialog-close focus restoration with automated GPUI UI regression tests (**13/13** passing on both macOS and Linux).
4. Completed upfront live verification of `target/release/quarry-gpui` inside both VMPal VMs:
   - **`macOS VM 1` (macOS 27.2.1 ARM64):** Live macOS Accessibility / AccessKit tree (`accesskit_macos 0.26.3`), native horizontal scroll + `Shift + vertical wheel`, single-click nested popover switching, and Cell Editor dialog focus restoration.
   - **`Omarchy VM 4` (Omarchy Linux ARM64, Hyprland 0.56.2):** Native Wayland client (`"xwayland": false`), Vulkan 1.4 surface creation via Mesa `26.2.5` Lavapipe, `cosmic-text` font resolution, `org.freedesktop.portal.FileChooser` v4 availability, and live `accesskit_unix 0.22.1` AT-SPI tree inspection over D-Bus.

`apps/quarry-egui` remains intact and remains the default packaging target in `scripts/macos-app.sh` until Phase 4B-3 cutover.

---

## 1. Environments and Inputs

- **macOS VM (`macOS VM 1`, VMPal):** macOS `27.2.1 (26B131)`, `aarch64-apple-darwin`, Rust `1.100.0` (`0ef1bc386`, LLVM `23.1.5`), display `1920x997` (`screenTree: true`, VMPal Tools `0.42`).
- **Linux / Omarchy VM (`Omarchy VM 4`, VMPal):** Omarchy Linux `7.3.0-3.1-omarchy aarch64`, Rust `1.100.0`, Hyprland `0.56.2` on `wayland-1`, output `Virtual-1` (`7680x2160@60Hz`, `scale = 2`), Mesa `26.2.5-arch1.1`, `vulkan-icd-loader 1.4.363.0-1`, `vulkan-virtio 1:26.2.5-1`, `vulkan-swrast 1:26.2.5-1`, `vulkan-tools 1.4.363.0-1`, AT-SPI bus at `unix:path=/run/user/1000/at-spi/bus_0`.
- **Synthetic live fixture:** `/tmp/quarry-gpui-verify.csv` — 501 rows (1 header row `col_0..col_39` + 500 data rows `r0_c0..r499_c39`), 40 columns, `0.16 MiB` (synthetic only; removed after testing).

---

## 2. Zero-HTTP / Zero-Transport Gate & Binary Sizes

### Dependency-graph proof (all 4 shipping targets)

Checked with `cargo tree -p quarry-gpui --locked --target <triple>` for:

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`

Results across all four targets:

- **Forbidden HTTP / TLS / socket transport crates:** **0** (`gpui-pre-reqwest`, `reqwest`, `hyper`, `hyper-util`, `h2`, `rustls`, `tokio-rustls`, `native-tls`, `openssl`, `quinn`, `ureq`, `curl`, `curl-sys`, `http-body`, `tower-http`, `surf`, `isahc`, `attohttpc` — none present).
- **Audited exceptions explicitly approved for Phase 4B-1:**
  - `gpui-pre-http-client 0.10.0`: pure trait/type crate pulled by `gpui-pre`; contains no HTTP transport implementation.
  - `oo7` (Linux targets only): local D-Bus secret-service client pulled by `gpui-pre-linux`; uses local UNIX socket D-Bus only (no network transport).

### Linked binary inspection & release sizes

| Target | Build | Unstripped | Stripped | Dynamic libraries | Forbidden symbols / strings |
|---|---|---:|---:|---|---|
| macOS ARM64 (`aarch64-apple-darwin`) | `cargo build -p quarry-gpui --release --locked` | **22 MiB** | **22 MiB** (`strip` removes 0 MiB; already release-stripped/minimal symbols) | Apple system frameworks & `/usr/lib/libSystem.B.dylib` / `libc++.1.dylib` only (`otool -L`) | **NONE** (`nm -u` & `strings` grep for `reqwest`/`hyper`/`rustls`/`libcurl`/`SSLCreateContext`) |
| Linux ARM64 (`aarch64-unknown-linux-gnu`) | `cargo build -p quarry-gpui --release --locked` | **45 MiB** | **33 MiB** | Standard libc/libm/libgcc/libxcb/libxkbcommon/libfontconfig/libfreetype (`ldd`; Wayland and Vulkan loaded via `libloading`) | **NONE** (`nm -D -u` & `strings` grep) |

### Third-party license pre-audit (`T19`)

Recorded in `packaging/licenses/AUDIT.md` (PR `#191`):

- `packaging/licenses/` was regenerated for Rust `1.100.0` runtime notices (`COPYRIGHT-library.html`, `COPYRIGHT-backtrace.html`, `THIRD_PARTY_NOTICES.html`, `manifest.json`, `reviewed.sha256`) while keeping shipping notices scoped to `quarry-egui` + `quarry-cli` until Phase 4B-3.
- Read-only pre-audit of `quarry-gpui`'s dependency graph found 11 crates using permissive or weak-copyleft licenses outside `quarry-egui`'s narrow allowlist (`0BSD`, `BSD-2-Clause`, `BSD-3-Clause`, `CC0-1.0`, and `MPL-2.0` via `option-ext@0.2.0`), and **0** GPL, LGPL, AGPL, or non-OSI crates.

---

## 3. Automated Quality & UI Regression Suite

Run on both `macOS VM 1` (`aarch64-apple-darwin`) and `Omarchy VM 4` (`aarch64-unknown-linux-gnu`) at commit `40ff4d2`:

```sh
cargo clippy -p quarry-gpui --all-targets --locked -- -D warnings
cargo test -p quarry-gpui --locked
cargo build -p quarry-gpui --release --locked
```

- **Clippy:** 0 warnings on both macOS and Linux.
- **Unit + GPUI UI tests:** **13/13 passed** on both macOS and Linux, including three new Phase 4B-1 regression tests in `apps/quarry-gpui/src/ui_tests.rs`:
  1. `horizontal_scroll_supports_pixels_lines_and_shift_wheel` — verifies horizontal scrolling via `ScrollDelta::Pixels(dx, 0)`, `ScrollDelta::Lines(dx, 0)`, and `Shift + vertical wheel` (`ScrollDelta::Lines(0, dy)` with `shift: true`).
  2. `nested_select_switches_directly_to_another_toolbar_panel_in_one_click_with_focus` — verifies opening `Format`, opening its nested `Delimiter` `Select`, and clicking `Columns` switches directly to the `Columns` popover in a single click with keyboard focus preserved (`Escape` dismisses cleanly).
  3. `accessibility_exposes_grid_cell_labels_and_restores_focus_after_editor` — verifies `TableDelegate::cell_label` formats gutter and data cells (`Row N`, `Column N: value`, `Column N: empty`) and closing the Cell Editor dialog restores keyboard focus to the `DataTable`.

---

## 4. Live macOS Verification (`macOS VM 1` — `T2` Accessibility & Quirks)

Launched `/Users/danielchamorro/Developer/quarry-4b1/target/release/quarry-gpui /tmp/quarry-gpui-verify.csv` inside `macOS VM 1` and inspected the live OS Accessibility tree via VMPal `vmpal_tree` (`accesskit_macos 0.26.3`):

1. **Lazy AccessKit activation & full semantic tree (33–52 ms tree reads):**
   - Window title: `quarry-gpui-verify.csv · Quarry GPUI comparison`
   - Toolbar: `toolbar "Document toolbar"` with all 9 controls (`button "quarry-gpui-verify.csv"`, `button "Format"`, `text field "Go to row" = "1" [editable]`, `button "Go"`, `button "Undo" [disabled]`, `button "Redo" [disabled]`, `button "Columns"`, `button "Filters"`, `button "Save a copy…"`).
   - Data grid: `table "quarry-gpui-verify.csv data grid"` exposing column headers (`#`, `Column 1`..`Column 10`), row gutter cells (`Row 1`..`Row 24`), and virtualized per-cell labels (`Column 1: col_0`, `Column 1: r0_c0`, …).
   - Status bar: `group "501 rows · 0.16 MiB · 40 of 40 columns · Index complete"`.
2. **Quirk #1 (Horizontal scrolling) — PASS:**
   - Native horizontal scroll (`vmpal_act` `scroll right 8`) shifted visible columns from `Column 1..10` to `Column 2..12`.
   - `Shift + vertical wheel` (`scroll down 6` with `modifiers: "shift"`) shifted visible columns further to `Column 4..14` (`Column 13`, `Column 14` visible in both screenshot and live Accessibility tree).
3. **Quirk #2 (Nested `Select` outside-click switch) — PASS:**
   - Clicked `button "Format"` -> verified Format popover Accessibility nodes (`Close Format`, `pop up button "Delimiter" = "Auto"`, `pop up button "Header row" = "Auto"`, `Cancel`, `Reopen [disabled]`).
   - Clicked `pop up button "Delimiter"` to open the nested `Select` menu, then clicked `button "Columns"` **once** -> `Columns` popover opened immediately on the first click (`Close Columns`, `text field "Search columns" [editable]`, `Reset`, `check box "Show 1: Column 1" [checked]`, `Move 1: Column 1 left [disabled]` / `right`, `Auto-fit columns`, `Done`).
   - Pressed `Escape` -> dismissed the `Columns` popover cleanly.
4. **Cell Editor dialog & focus restoration — PASS:**
   - Double-clicked cell `Column 5: r0_c4` (row 2) -> verified modal Accessibility nodes (`text area "Cell value" = "r0_c4" [editable]`, `button "Cancel"`, `button "Apply edit"`, `button "Close"`).
   - Clicked `button "Cancel"`, then pressed `Down` arrow with no intermediate click -> active cell selection moved directly from row 2 (`r0_c4`) to row 3 (`r1_c4`), proving keyboard focus restored reliably to the `DataTable`.

---

## 5. Live Linux / Omarchy Verification (`Omarchy VM 4` — `T4` Wayland, Vulkan, AT-SPI)

### Finding during setup: Vulkan ICD prerequisite

`Omarchy VM 4` originally had `mesa 1:26.2.5-1` and `vulkan-icd-loader 1.4.363.0-1` installed with `/usr/share/vulkan/icd.d/` empty (zero Vulkan ICD packages installed), causing `wgpu` surface creation to fail with `No GPU adapter found that can configure the display surface`. Installing `vulkan-virtio`, `vulkan-swrast`, and `vulkan-tools` populated `lvp_icd.json` and `virtio_icd.json` (with `llvmpipe` Vulkan `1.4.354`, `VK_KHR_wayland_surface` rev 6), after which `target/release/quarry-gpui` launched cleanly on Hyprland.

*Action item for Phase 4B-2 / Phase 9B:* Document Vulkan ICD requirements for Linux packages/AUR and improve startup diagnostics if no Vulkan adapter is found.

### Live verification results

Launched `~/Developer/quarry/target/release/quarry-gpui /tmp/quarry-gpui-verify.csv` inside Hyprland (`wayland-1`):

1. **Native Wayland (`xwayland: false`) — PASS:**
   - `hyprctl clients -j`: `class: "dev.quarry.gpui-comparison"`, `title: "quarry-gpui-verify.csv · Quarry GPUI comparison"`, **`"xwayland": false`**, `size: [1901, 1030]` on workspace 2 (`Virtual-1`, `7680x2160@60Hz`, `scale = 2`), RSS `374–405 MiB` (includes software `llvmpipe` LLVM JIT/framebuffer memory in the VM).
   - Loaded shared libraries (`/proc/<pid>/maps`): `/usr/lib/libwayland-client.so.0.26.0`, `/usr/lib/libvulkan.so.1.4.363`, `/usr/lib/libvulkan_lvp.so`, `/usr/lib/libxkbcommon.so.0.13.2`, `/usr/lib/libVkLayer_MESA_device_select.so`.
2. **Typography (`cosmic-text` / fontconfig) — PASS:**
   - Resolved system fonts: `Liberation Sans` (`LiberationSans-Regular.ttf`) and `JetBrainsMono Nerd Font` (`JetBrainsMonoNerdFont-Regular.ttf`).
3. **XDG desktop portal (`ashpd`) — PASS (service verified):**
   - `org.freedesktop.portal.Desktop` active on session D-Bus with `org.freedesktop.portal.FileChooser` version `4` (`OpenFile`, `SaveFile`, `SaveFiles`).
4. **Live Linux AT-SPI (`accesskit_unix 0.22.1`) — PASS:**
   - Setting `org.a11y.Status.IsEnabled = true` on `org.a11y.Bus` caused the already-running `quarry-gpui` process to connect live to the AT-SPI bus (`unix:path=/run/user/1000/at-spi/bus_0`, bus name `:1.9`) without restart.
   - Live D-Bus tree inspection verified:
     - `Application` (`role#75`): `'quarry-gpui'`
     - `Frame` (`role#23`): `'quarry-gpui-verify.csv · Quarry GPUI comparison'`
     - `ToolBar` (`role#63`): `'Document toolbar'` (9 children matching macOS toolbar)
     - `Table` (`role#55`): `'quarry-gpui-verify.csv data grid'` (32 virtualized rows, `ColumnHeader` `role#10` for `#` and `Column 1`..`Column 17`, `TableRow` `role#90`, `TableCell` `role#56` for `'Row 1'`, `'Column 1: col_0'`, …)
     - `StatusBar` (`role#54`): `'501 rows   ·   0.16 MiB   ·   40 of 40 columns · Index complete'`

### Scoped follow-ups (tracked honestly for Phase 4B-2 / Phase 6 / Phase 9B)

- **AT-SPI row summary concatenation:** In `gpui-component 0.10.0`, `TableRow`'s synthesized row label calls `cell_label` across all columns, so columns outside the currently fetched horizontal viewport (`Column 19..40` on a 40-column file) render as `empty` in the row summary while visible `TableCell` nodes expose accurate values. Tracked for refinement in Phase 4B-2.
- **Visual fractional-scaling (`1.25` / `1.5`) pixel check & hardware Vulkan GPU frame pacing:** `Omarchy VM 4` had its session locker (`hyprlock`) active during headless verification and ran on Mesa `llvmpipe` software Vulkan (no VirtIO hardware Vulkan pass-through yet). Unlocked visual inspection at `1.25×` / `1.5×` fractional scale, interactive XDG portal picker round-trip, and hardware Vulkan GPU frame pacing / RSS measurement remain open for Phase 4B-2 / Phase 6 / Phase 9B (`C07`, `C08`, `C09`, `C42`).
