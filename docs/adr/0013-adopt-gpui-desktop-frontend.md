# ADR 0013: Adopt GPUI as Quarry's v1.0 desktop frontend

**Status:** Accepted (phased migration; `apps/quarry-egui` remains the default packaged macOS app until Phase 4B-3 cutover)
**Date:** 2026-11-23
**Supersedes:** [ADR 0003: Select egui for the production UI](0003-select-egui-ui.md)

## Context

[ADR 0003](0003-select-egui-ui.md) chose `egui` (`apps/quarry-egui`) for Quarry's initial viewer alpha and beta releases because it delivered a safe-Rust virtualized grid with per-cell VoiceOver exposure quickly while keeping `quarry-core` UI-independent. That choice served betas 1 through 3 well, but Quarry's v1.0 requirements expose structural limits in `egui`'s immediate-mode desktop architecture:

- **Native multi-window and 200-tab workbench ergonomics:** Managing independent desktop windows, native menu bars, command palettes, drawers, and high-frame-rate scrolling cleanly across macOS and Linux/Wayland requires retained GPU rendering and native windowing primitives.
- **Native Wayland / Hyprland / Omarchy support:** Quarry's v1.0 target includes native Wayland (`xwayland: false`) on Linux/Omarchy with `cosmic-text` typography, XDG file portals (`ashpd`), and Linux AT-SPI accessibility alongside macOS Metal + CoreText + VoiceOver.
- **High-refresh-rate headroom:** Smooth rendering on 120 Hz displays requires incremental GPU scene submission rather than repainting the entire immediate-mode UI tree on every interaction.
- **Strict local-only / zero-network posture:** Any replacement frontend must preserve Quarry's zero-HTTP, zero-telemetry guarantee (no HTTP, TLS, or network transport crates in shipping target dependency graphs or linked binaries) and dual MIT/Apache-2.0 licensing compatibility.

During Phase 4A (`#184`–`#188`), Quarry extracted its UI-independent document model, tab/command state, Light semantic palette tokens, and dormant-tab/FD-budget scheduler into `crates/quarry-workbench` (zero GUI dependencies). Phase 4B-1 (`#189`, `#190`) hardened `apps/quarry-gpui` inside the root Cargo workspace and verified every blocking technical gate before committing to full feature parity work.

## Decision

Adopt **GPUI** via **`gpui-kit = "=0.10.0"`** (`gpui-pre` / `gpui-component` / `gpui-base`, pinned by exact version in the root workspace `Cargo.toml` and `Cargo.lock`) as Quarry's v1.0 desktop frontend in `apps/quarry-gpui`, built over the frontend-neutral `crates/quarry-core` and `crates/quarry-workbench` crates, targeting **macOS (Metal / CoreText / AccessKit-macOS)** and **Linux / Wayland / Omarchy (Vulkan / `cosmic-text` / `ashpd` / AccessKit-AT-SPI)**.

Key architectural constraints enforced by this decision:

1. **Workspace and MSRV unification:** `apps/quarry-gpui` is a first-class member of the root Cargo workspace sharing the single root `Cargo.lock` and pinned Rust toolchain (`1.100.0`, LLVM `23.1.5`).
2. **Hard Zero-HTTP / Zero-Transport invariant:** `apps/quarry-gpui` depends on `gpui-kit = { version = "=0.10.0", default-features = false, features = ["icons"] }` so default transport/network features (`reqwest` / `gpui-pre-reqwest`) are never enabled. Zero HTTP/TLS/socket transport crates (`gpui-pre-reqwest`, `reqwest`, `hyper`, `rustls`, `quinn`, `native-tls`, `libcurl`, `ureq`) may appear in shipping target graphs or linked release binaries. The pure-trait `gpui-pre-http-client` crate (no transport backend) and Linux-only local D-Bus `oo7` dependency pulled by `gpui-pre-linux` are explicitly audited and permitted so long as no network transport or socket I/O is linked.
3. **Mandatory accessibility parity (`T2`):** Both macOS VoiceOver (`accesskit_macos`) and Linux AT-SPI (`accesskit_unix`) must expose semantic roles (`Toolbar`, `Grid`/`Table`, column headers, per-cell `GridCell` labels, `Status`/`Alert` banners, popover/dialog controls, and reliable keyboard focus restoration after dialogs close). Accessibility regressions are release blockers.
4. **Phased cutover keeping `apps/quarry-egui` shipping until Phase 4B-3:** `apps/quarry-egui` remains intact and remains the default packaging target in `scripts/macos-app.sh` throughout Phase 4B-1 and Phase 4B-2. Packaging cutover happens only in Phase 4B-3 after full feature parity, 200-tab / multi-window workbench verification, accessibility verification, and refreshed third-party notices (`T19`) pass owner sign-off.
5. **Dual-platform desktop scope (`T4`) and theme readiness (`T3`):** macOS and Linux/Omarchy are both first-class GPUI desktop targets. Light theme tokens remain shared via `quarry_workbench::theme`, with Dark and Omarchy theme integration authorized for Phase 4B-2 / Phase 6.

## Evidence (Phase 4B-1 Verification)

Full raw measurements and environment details are recorded in [2026-11-23-phase4b1-gpui-hardening.md](../benchmarks/2026-11-23-phase4b1-gpui-hardening.md) and [apps/quarry-gpui/VALIDATION.md](../../apps/quarry-gpui/VALIDATION.md):

| Gate | Venue | Result |
|---|---|---|
| Workspace & MSRV unification (`#191`) | macOS ARM64 (`macOS VM 1`) + Linux ARM64 (`Omarchy VM 4`) + GitHub CI (macOS/Linux/Windows) | `apps/quarry-gpui` unified on `gpui-kit = "=0.10.0"`, Rust `1.100.0`, single `Cargo.lock`; all 7 workspace crates clean under `cargo clippy --all-targets --locked -- -D warnings`. |
| Zero-HTTP / Zero-Transport proof (`#191`) | `cargo tree` across `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` + linked release binaries (`otool -L`, `ldd`, `nm`, `strings`) | **PASS.** Zero forbidden HTTP/TLS/socket transport crates or linked symbols on macOS or Linux. |
| Release binary size (`C10`) | `macOS VM 1` (ARM64) & `Omarchy VM 4` (ARM64) | macOS ARM64 `quarry-gpui`: **22 MiB** (stripped & unstripped); Linux ARM64 `quarry-gpui`: **45 MiB** unstripped / **33 MiB** stripped (compared to ~5.2 MiB for `quarry-egui`). |
| Prototype quirks & automated UI suite (`#190`) | `macOS VM 1` & `Omarchy VM 4` | **13/13** `quarry-gpui` tests pass on both macOS and Linux, covering native horizontal wheel (`Pixels`/`Lines`) + `Shift + vertical wheel`, single-click switch from nested `Format` `Select` to `Columns`, and grid cell accessibility labels + dialog focus restoration. |
| Live macOS Accessibility / VoiceOver (`T2`) | `macOS VM 1` (macOS 27.2.1, VMPal `vmpal_tree` / `accesskit_macos 0.26.3`) | **PASS.** Live macOS Accessibility tree reads in 33–52 ms exposing `Document toolbar`, all buttons/disabled states, `Go to row` text field, `quarry-gpui-verify.csv data grid` table, column headers, row numbers, per-cell `Column N: value` labels, status bar, `Format`/`Columns` popovers, and `Cell value` dialog. Live `dx` scroll, `Shift + vertical wheel`, one-click `Delimiter`->`Columns` switch, and dialog `Cancel` -> `Down` focus restore all verified. |
| Live Linux / Omarchy Wayland + Vulkan + AT-SPI (`T4`) | `Omarchy VM 4` (Omarchy `7.3.0-3.1-omarchy aarch64`, Hyprland `0.56.2`, Mesa `26.2.5` Lavapipe Vulkan 1.4) | **PASS (with scoped follow-ups).** Native Wayland (`"xwayland": false`, `libwayland-client.so.0.26.0`), Vulkan 1.4 (`libvulkan.so.1.4.363` + `libvulkan_lvp.so`), `cosmic-text` fontconfig (`Liberation Sans` / `JetBrainsMono Nerd Font`), `org.freedesktop.portal.FileChooser` v4 active, and live `accesskit_unix 0.22.1` AT-SPI tree (`Application` -> `Frame` -> `ToolBar`, `Table` with headers/rows/cells, `StatusBar`). Visual pixel check of `1.25`/`1.5` fractional scaling and hardware Vulkan GPU frame pacing remain tracked for Phase 4B-2 / Phase 6 / Phase 9B on an unlocked hardware-GPU session. |
| License pre-audit (`T19`) | `packaging/licenses/AUDIT.md` | 11 `quarry-gpui`-only crates use permissive/weak-copyleft licenses outside `quarry-egui`'s allowlist (`0BSD`, `BSD-2-Clause`, `BSD-3-Clause`, `CC0-1.0`, and `MPL-2.0` via `option-ext@0.2.0`); zero GPL/LGPL/AGPL/non-OSI crates. Notices will be regenerated for `quarry-gpui` at Phase 4B-3 cutover. |

## Consequences

- **Phase 4B-2** will port full desktop feature parity, multi-window / 200-tab `WorkbenchScheduler` integration, remaining modals/dialogs, and native menus into `apps/quarry-gpui` while keeping `apps/quarry-egui` buildable and shipping.
- **Phase 4B-3** will switch `scripts/macos-app.sh` packaging and `packaging/licenses/` shipping notices to `quarry-gpui`, run side-by-side macOS + Linux verification (including 120 Hz hardware frame pacing in Phase 6/9), and retire `apps/quarry-egui` only after owner sign-off.
- **Upgrade discipline:** `gpui-kit` remains exact-pinned (`=0.10.0`). Any version bump must re-run the four-target `cargo tree` + linked-binary Zero-HTTP audit, both macOS and Linux accessibility checks, and notice audit.
- **Linux packaging note:** Linux/Omarchy requires a working Vulkan ICD (`vulkan-virtio`, hardware Mesa/NVIDIA/Intel/AMD Vulkan driver, or `vulkan-swrast` fallback) for `wgpu` surface creation; packaging docs and runtime error diagnostics in Phase 4B-2 / Phase 9B must surface clear guidance if no Vulkan ICD is installed.
- **Abort / rollback path:** Until Phase 4B-3 cutover completes and is signed off, `apps/quarry-egui` remains intact on `main` and in release scripts. If an unresolvable regression arises in accessibility, memory ceiling, or platform stability during Phase 4B-2, Quarry can continue shipping `apps/quarry-egui` without disrupting users.
