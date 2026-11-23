# GPUI frontend validation

## Phase 4B-1 hardening and live VM verification (2026-11-23)

`apps/quarry-gpui` is now a unified member of the root Cargo workspace on `gpui-kit = "=0.10.0"` and Rust `1.100.0` (PR `#191`), and [ADR 0013](../../docs/adr/0013-adopt-gpui-desktop-frontend.md) records the decision to adopt GPUI as Quarry's v1.0 desktop frontend while keeping `apps/quarry-egui` as the default packaged macOS app until Phase 4B-3 cutover.

Full Phase 4B-1 evidence is recorded in [docs/benchmarks/2026-11-23-phase4b1-gpui-hardening.md](../../docs/benchmarks/2026-11-23-phase4b1-gpui-hardening.md):

- **Zero-HTTP / Zero-Transport gate:** Verified across all four shipping target graphs (`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`) and linked release binaries (`otool -L`, `ldd`, `nm`, `strings`).
- **Release binary sizes:** macOS ARM64 `22 MiB` (stripped and unstripped); Linux ARM64 `45 MiB` unstripped / `33 MiB` stripped.
- **Automated checks (macOS ARM64 & Linux ARM64):** `cargo clippy -p quarry-gpui --all-targets --locked -- -D warnings` (0 warnings) and `cargo test -p quarry-gpui --locked` (**13/13** passing).
- **Prototype quirks resolved and verified live in `macOS VM 1`:**
  1. **Horizontal scrolling:** Native horizontal scroll (`ScrollDelta::Pixels` / `ScrollDelta::Lines` `dx`) and capture-phase `Shift + vertical wheel` both move the table horizontally in automated UI tests and live macOS VM runs.
  2. **Nested `Select` outside-click focus:** Opening `Format` -> `Delimiter` (`Select`) and clicking another toolbar popover (`Columns`) switches in a single click with keyboard focus preserved (`Escape` dismisses cleanly).
- **Live accessibility verification:**
  - **macOS (`macOS VM 1`, `accesskit_macos 0.26.3`):** Verified live toolbar, disabled states, editable fields, `DataTable` column headers, row numbers, per-cell `Column N: value` labels, status bar, `Format`/`Columns` popovers, and Cell Editor dialog-close focus restoration back to the grid.
  - **Linux / Omarchy (`Omarchy VM 4`, Hyprland `0.56.2`, `accesskit_unix 0.22.1`):** Verified native Wayland (`"xwayland": false`), Vulkan 1.4 surface creation (Mesa `26.2.5` Lavapipe), `cosmic-text` font resolution, `org.freedesktop.portal.FileChooser` v4 availability, and live AT-SPI tree exposure (`Application`, `Frame`, `ToolBar`, `Table`, `ColumnHeader`, `TableRow`, `TableCell`, `StatusBar`).
- **Remaining work tracked for Phase 4B-2 / 4B-3 / 6 / 9B:** Full desktop feature parity with `apps/quarry-egui`, 200-tab / multi-window `WorkbenchScheduler` wiring, Dark and Omarchy themes, AT-SPI off-viewport row-label refinement, unlocked visual `1.25×`/`1.5×` fractional-scaling and hardware Vulkan / 120 Hz frame-pacing verification, and Phase 4B-3 packaging + `T19` shipping notice cutover.

---

## Original local prototype comparison (2026-09-10)

Historical baseline recorded when evaluating the standalone prototype before Phase 4B-1 workspace unification:

### Environment and inputs

- Apple M3 Max, 128 GiB memory, macOS 26.6.2 (25G83).
- GPUI Kit 0.6.1, Rust 1.97.1, optimized release build.
- Existing `quarry-core` used through a path dependency without changes.
- Synthetic large file: 1,000,000 data rows, 42 columns, 276,689,405 bytes.
- Synthetic wide file: 10,000 data rows, 300 columns, 31,601,501 bytes.

### Prototype observations (archived)

- Opened both files through the native file picker and completed indexing.
- Format's Header choice reopened the large file with its first row as headers, leaving 1,000,000 data rows.
- Jumped near the end of the million-row file and displayed the final rows.
- Applied `State contains NV` (500,000 of 1,000,000 rows, `Filter complete`) and cleared cleanly.
- Verified wide-file Home/End navigation across 300 columns, column hide/reorder/auto-fit, cell editing, safe Save Copy, Undo, Cmd+O at launch, and unsaved-draft close guard.
- The two quirks noted in this original run (native horizontal scroll and nested `Select` outside-click panel switching) were fixed and verified in Phase 4B-1 above.
