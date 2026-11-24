# ADR 0013: Adopt GPUI as Quarry's v1.0 desktop frontend

**Status:** Accepted & Active (Phase 4B-3 macOS packaging and third-party notice cutover complete; `apps/quarry-egui` retained intact in the workspace for rollback until explicit post-cutover retirement sign-off)
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

## Evidence (Phase 4B-1 through Phase 4B-3 Cutover Verification)

Full raw measurements and environment details are recorded in [2026-11-23-phase4b1-gpui-hardening.md](../benchmarks/2026-11-23-phase4b1-gpui-hardening.md), [2026-11-23-phase4b2-gpui-workbench-verification.md](../benchmarks/2026-11-23-phase4b2-gpui-workbench-verification.md), [2026-11-23-phase4b3-gpui-cutover-verification.md](../benchmarks/2026-11-23-phase4b3-gpui-cutover-verification.md), and [apps/quarry-gpui/VALIDATION.md](../../apps/quarry-gpui/VALIDATION.md):

| Gate | Venue | Result |
|---|---|---|
| Workspace & MSRV unification (`#191`) | macOS ARM64 (`macOS VM 1`) + Linux ARM64 (`Omarchy VM 4`) + GitHub CI (macOS/Linux/Windows) | `apps/quarry-gpui` unified on `gpui-kit = "=0.10.0"`, Rust `1.100.0`, single `Cargo.lock`; all 7 workspace crates clean under `cargo clippy --all-targets --locked -- -D warnings`. |
| Zero-HTTP / Zero-Transport & Offline Socket proof (`#191`, `#202`) | `cargo tree` across `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` + linked release binaries (`otool -L`, `ldd`, `nm`) + live `lsof -i` / `ss -tupn` | **PASS.** Zero forbidden HTTP/TLS/socket transport crates or linked symbols on macOS or Linux; `0` live open network sockets during live sessions on both `macOS VM 1` and `Omarchy VM 4`. |
| Full Feature Parity, Multi-Window & 200-Tab Workbench (`#197`–`#200`, `#203`) | `macOS VM 1` & `Omarchy VM 4` | **PASS.** 100% `quarry-egui` feature parity ported over `crates/quarry-workbench` (native multi-window, 200-tab dormancy at `141.39–153.02 MiB` macOS RSS / `351.13 MiB` Linux `VmHWM` and `< 128` FDs, Find/Replace, Filters, Multi-column Sort, Duplicates, Cleanup, Structural Transforms, Combine/Join/Inspect/Recovery, `.gz`, WCAG AA Light/Dark/Omarchy themes, `tnum`, macOS install lock, `on_open_urls`/`on_reopen`, bundled `Info.plist` About dialog). `23/23` `quarry-gpui` UI tests + `95` `quarry-workbench` tests passing on both platforms. |
| Release binary size & warm launch latency (`C10`) | `macOS VM 1` (ARM64) & `Omarchy VM 4` (ARM64) | Full-parity macOS ARM64 `quarry-gpui`: **26.39 MiB** (`27,675,376` B, vs `9.90 MiB` `quarry-egui`); Linux ARM64 `quarry-gpui`: **48.75 MiB** unstripped / **36.01 MiB** stripped. Installed `/Applications/Quarry.app` warm launch-to-onscreen-window median: **139.3 ms** (`min 136.1 ms`, `max 140.2 ms`, first run `157.0 ms`). |
| Packaging Cutover, Verified Rollback & Live macOS Accessibility (`#202`, `T2`) | `macOS VM 1` (`macOS 27.2.1`, build `26B131`) | **PASS.** `scripts/macos-app.sh` switched to `quarry-gpui` with verified `rollback` subcommand (`self-test` passed). Live cutover (`egui` → `gpui`), live `rollback` (`gpui` → `egui`), live re-install (`gpui`), install-lock contention guard, Finder hard-link window routing, About Quarry `Info.plist` / clipboard, and live `accesskit_macos` tree (18–98 ms) all verified. |
| Live Unlocked Linux / Omarchy Wayland, Fractional Scaling & AT-SPI (`#202`, `T2`, `T4`) | `Omarchy VM 4` (`Omarchy 7.3.0-3.1-omarchy`, Hyprland `0.56.2`, Mesa `26.2.5`) | **PASS.** Unlocked native Wayland (`"xwayland": false`), live Omarchy `tokyo-night` sync, crisp `1.2x` (`1.25x` request), `1.5x`, and `2.0x` fractional scaling screenshots with zero blur or layout breakage, and live `accesskit_unix` AT-SPI tree (`64` semantic nodes). Bare-metal 120 Hz / hardware Vulkan frame-deadline tracing remains tracked for Phase 6 / Phase 9B (`T18`). |
| Shipping Third-Party Notices (`#202`, `T19`) | `packaging/licenses/` (`cargo-about 0.9.2`, Rust `1.100.0`) | **PASS.** Notices regenerated from `apps/quarry-gpui/Cargo.toml` for `397` locked packages (`about.toml` allowlist + `33` hash-verified `supplemental.json` entries including `gpui-kit-assets@0.10.0/LICENSE-LUCIDE`; zero GPL/LGPL/AGPL/non-OSI crates; `--check`, `--release-check`, and `17/17` tests pass). |

## Consequences

- **Active shipping desktop frontend:** `apps/quarry-gpui` is now the default macOS `.app` packaging target in `scripts/macos-app.sh` and the active notice target in `packaging/licenses/`.
- **Rollback safety & deferred retirement:** `scripts/macos-app.sh rollback` restores verified pre-cutover `.app` backups (`Quarry-previous.zip`), and `apps/quarry-egui` remains intact in the Cargo workspace until explicit post-cutover owner sign-off authorizes its removal.
- **Upgrade discipline:** `gpui-kit` remains exact-pinned (`=0.10.0`). Any version bump must re-run the four-target `cargo tree` + linked-binary Zero-HTTP audit, both macOS and Linux accessibility checks, and notice audit.
- **Linux packaging note:** Linux/Omarchy requires a working Vulkan ICD (`vulkan-virtio`, hardware Mesa/NVIDIA/Intel/AMD Vulkan driver, or `vulkan-swrast` fallback) for `wgpu` surface creation; Phase 9B Linux packaging docs and runtime diagnostics will surface clear guidance if no Vulkan ICD is installed.
- **Next phases:** Phase 5+ desktop features (Large File Controller, Live Watch Mode, Focus Mode, Command Palette, Cell Inspector, Visual Join Preview, Pivot Tables) build directly on `apps/quarry-gpui` and `crates/quarry-workbench`.
