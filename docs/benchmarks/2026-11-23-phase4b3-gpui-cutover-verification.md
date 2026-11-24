# Phase 4B-3 GPUI Packaging Cutover, Rollback & Dual-Platform Verification

**Date:** `2026-11-23`  
**Scope:** Phase 4B-3 (`#201` / PR `#203` — macOS OS integration & decisive multi-window cutover tests; `#202` — single packaging & third-party notice switch, verified rollback, and live dual-platform verification)  
**Verified Commit:** `3523acb172bb12f79ae2e7cc8bb509e4e577fc94` (`feat/phase4b3b-gpui-packaging-cutover`, building on merged `3f0769a`)  
**Workspace Retention Policy:** `apps/quarry-gpui` is now the active packaged macOS `.app` target in `scripts/macos-app.sh` and the active shipping notice target in `packaging/licenses/`. `apps/quarry-egui` remains intact in the Cargo workspace for rollback and side-by-side comparison until explicit post-cutover retirement sign-off.

---

## 1. Versioned Behavior-Parity Checklist (`apps/quarry-gpui` vs `apps/quarry-egui`)

Every user-facing capability shipped in `apps/quarry-egui` (through Beta 3 + Phase 4A) was audited and verified in `apps/quarry-gpui` before switching `scripts/macos-app.sh`:

| Area | Capabilities Verified in `apps/quarry-gpui` | Verification Evidence |
|---|---|---|
| **Progressive open & bounded grid** | Immediate bootstrap viewport, background parallel structural indexing, bounded virtualized scrolling (`Shift + vertical wheel` + native horizontal scroll), row-number gutter, Go to Row, open-file summary & status bar | Automated (`ui_tests.rs`), live `macOS VM 1` & `Omarchy VM 4` |
| **Format & character encoding** | Auto/explicit delimiter & header row modes, Auto/UTF-8/UTF-16 LE & BE/Windows-1252/ISO-8859-1 encoding selection, live reload without losing format choice | Automated (`ui_tests.rs`, `quarry-workbench`), PR `#197` |
| **Multi-window & 200-tab workbench** | Unlimited tabs up to 200+ via `WorkbenchScheduler` dormant-tab eviction (`< 128` FDs), native multi-window (`New Window`, `Close Window`, `Move to Window`), single-process cross-window `(dev, ino)` hard-link deduplication | Automated (`two_hundred_tabs_across_four_windows_stay_bounded_and_preserve_dirty_work`, `external_open_routes_hard_link_to_owning_window_without_duplicate`), live 200-file runs (`#200`) + live hard-link routing (`macOS VM 1`) |
| **Inline editing, history & persistence** | Sparse cell & header edits, multiline editor, Rename Selected Column Header, bounded individual Undo/Redo + one-level structural Undo/Redo, Save, Save As…, Save As UTF-8…, Reload from Disk, Discard Changes, dirty-tab close prompt & unified cross-window Quit review | Automated (`dirty_tab_survives_window_close_and_keeps_edits`, `quit_cancellation_on_second_dirty_tab_preserves_undo_redo_history`, `save_as_survives_focus_change_across_windows_and_tabs`), PR `#197`, `#203` |
| **Search, filtering & export** | Inline Find & Replace bar (`Cmd/Ctrl+F`, `Cmd/Ctrl+G`, `Shift+Cmd/Ctrl+G`, `Cmd+Alt+F` / `Ctrl+H`), Match case, Replace in Cell, Replace All, multi-rule text & numeric Filters, live cancelable filter & **Export Filtered…** | Automated (`ui_tests.rs`, `quarry-workbench`), PR `#198` |
| **Sorting, duplicates & cleanup** | Multi-column Sort (1–16 levels: Text, Number, Date/Time with bounded format suggestions, Natural, Reverse, Shuffle) with disk-space preflight; Find & Remove Duplicates (key columns or whole row) with Duplicate Preview confirmation; Clean Selected Columns (Trim, Collapse spaces, Uppercase, Lowercase, Fill Empty) | Automated (`ui_tests.rs`, `quarry-workbench`), PR `#198` |
| **Structural row/column transforms & column views** | Show/hide/reorder/reset column view (`Columns` popover), Insert Rows/Columns, Delete Selected Rows/Columns, Move Selected Columns, Split Selected Column (with storage review), Combine Selected Columns, bounded Copy Selection, Show All Whitespace | Automated (`ui_tests.rs`), PR `#197`, `#198` |
| **Multi-file workflows, `.gz` & recovery** | Combine Files, 4-mode Join Files (`Left`/`Inner`/`Right`/`Full Outer`, 1–16 keys, Preview, `DocumentLease`), `.gz` Preview/Decompress/Cancel/Save As uncompressed + early `.gz` output rejection, Inspect File & Recovery (active tab, failed-open, arbitrary path, raw `.gz`), Temporary Storage & Sidecar Index Cache settings | Automated (`ui_tests.rs`, `quarry-workbench`), PR `#199` |
| **macOS OS integration & About Quarry** | Startup shared non-blocking `flock` install lock (`/private/tmp/io.github.danchamorro.quarry.<uid>.install.lock`), startup `RLIMIT_NOFILE` raise, LaunchServices `-psn_*` filtering, `on_open_urls` (`file://` UTF-8-safe decode, pre-startup queue, editing-window guard), `on_reopen`, `on_app_quit` cleanup, bundled `Info.plist` metadata in **About Quarry** + **Copy Build Details** | Automated (`crates/quarry-core/src/source_stamp.rs`, `ui_tests.rs`, PR `#203`), live `macOS VM 1` |
| **Themes & typography** | System, Light, WCAG AA Dark, hardened Omarchy local theme sync (`{ ~/.config/omarchy, ~/.local/share/omarchy, ~/.local/state/omarchy }` allowlist, symlink-escape & `64 KiB` cap, live generation reload), OpenType `tnum` right-aligned numeric figures | Automated (`quarry-workbench`, `ui_tests.rs`, PR `#200`), live `macOS VM 1` & unlocked `Omarchy VM 4` |
| **Accessibility (`T2`)** | `accesskit_macos` (macOS VoiceOver) and `accesskit_unix` (Linux AT-SPI): native/in-window menus, `Open documents` tab strip, `Document toolbar`, `data grid` headers/rows/per-cell labels, status/alert banners, modal dialog focus restoration | Automated (`ui_tests.rs`), live `vmpal_tree` on `macOS VM 1` & live AT-SPI D-Bus walk on unlocked `Omarchy VM 4` |

---

## 2. Zero-HTTP, Linked-Binary, Offline Socket & Notice Audit (`C01`, `C39`, `C40`, `T19`)

### 2.1 Four-Target Cargo Dependency Graph (`cargo tree -p quarry-gpui --locked`)

Verified on `macOS VM 1` at `3523acb` across all four desktop target triples:

| Target Triple | Forbidden HTTP/TLS/Socket Crates (`gpui-pre-reqwest`, `reqwest`, `hyper`, `rustls`, `quinn`, `native-tls`, `libcurl`, `ureq`) | Electron / Chromium / JS / GC Runtimes | Result |
|---|---|---|---|
| `aarch64-apple-darwin` | `0` | `0` | **PASS** |
| `x86_64-apple-darwin` | `0` | `0` | **PASS** |
| `x86_64-unknown-linux-gnu` | `0` | `0` | **PASS** |
| `aarch64-unknown-linux-gnu` | `0` | `0` | **PASS** |

Audited exceptions (retained per ADR 0013): `gpui-pre-http-client` (pure traits only, zero transport backend) and Linux-only local D-Bus `oo7` pulled by `gpui-pre-linux` (zero outbound network sockets).

### 2.2 Linked Release Binary & Live Offline Network Socket Audits

| Venue | Artifact / PID | Dynamic Libraries (`otool -L` / `ldd`) | Undefined Forbidden HTTP/TLS Symbols (`nm`) | Live Open Network Sockets (`lsof -nP -i` / `ss -tupn`) |
|---|---|---|---|---|
| `macOS VM 1` (`macOS 27.2.1`, build `26B131`, `arm64`) | `/Applications/Quarry.app/Contents/MacOS/quarry-gpui` (PID `78304`) | Apple system frameworks (`Metal`, `AppKit`, `CoreText`, `CoreGraphics`, `QuartzCore`, `Security`, etc.) + `libSystem.B.dylib`, `libobjc.A.dylib`, `libiconv.2.dylib` only | `NONE` | `NONE` (`0` sockets across launch, 2-window file open, hard-link routing, and About Quarry) |
| `Omarchy VM 4` (`Omarchy 7.3.0-3.1-omarchy`, `aarch64`) | `target/release/quarry-gpui` (PID `66990`) | Standard system Linux/Wayland/Vulkan/Fontconfig/glibc libraries only | `NONE` | `NONE` (`0` sockets across native Wayland launch, Omarchy theme sync, multi-tab view, and AT-SPI inspection) |

### 2.3 Shipping Third-Party Notices Cutover (`packaging/licenses/`)

- Regenerated `packaging/licenses/{THIRD_PARTY_NOTICES.html, manifest.json, reviewed.sha256, AUDIT.md}` directly from `apps/quarry-gpui/Cargo.toml` with checksum-verified `cargo-about 0.9.2` (`SHA-256: ae72f0df0c399a1e96336f696fa55b1b28679fd725632eba8cf8e4568467cc3e`) for **`397` locked packages** (`aarch64-apple-darwin`, Rust `1.100.0`).
- Expanded `packaging/licenses/about.toml` with the 5 pre-audited OSI licenses (`0BSD`, `BSD-2-Clause`, `BSD-3-Clause`, `CC0-1.0`, `MPL-2.0`; zero GPL/LGPL/AGPL/non-OSI crates).
- Rebuilt `packaging/licenses/supplemental.json` with **33** hash-verified notices covering every `cargo-about` fallback crate plus `gpui-kit-assets@0.10.0/LICENSE-LUCIDE` (`SHA-256: b495047bd93a9b06913511076f504daba17d5bbeb3e0650f3bb53a4220329c57`) and crate `COPYRIGHT` files (`rustix`, `unicode-segmentation`, `utf8_iter`), with zero bundled runtime fonts.
- Verification: `./scripts/generate-notices.sh --check --target aarch64-apple-darwin --rust-release 1.100.0`, `./scripts/generate-notices.sh --release-check`, and `python3 -m unittest packaging/licenses/test_generate.py` (`17/17` tests) all passed.

---

## 3. Release Binary Sizes & Launch Latency (`C10`)

| Metric | Venue | Measurement | Notes |
|---|---|---|---|
| **macOS ARM64 `quarry-gpui` release binary** | `macOS VM 1` (`arm64`) | **`26.39 MiB`** (`27,675,376` B) | Signed inside `/Applications/Quarry.app` (vs `9.90 MiB` / `10,351,696` B for prior `quarry-egui` build `b3afd22` and `22 MiB` for Phase 4B-1 prototype) |
| **Linux ARM64 `quarry-gpui` release binary** | `Omarchy VM 4` (`aarch64`) | **`48.75 MiB`** (`51,120,664` B) unstripped / **`36.01 MiB`** (`37,763,696` B) stripped | Static Rust + GPUI + `wgpu`/Vulkan + `cosmic-text` |
| **macOS installed `.app` launch-to-onscreen-window** | `macOS VM 1` (`arm64`, `/Applications/Quarry.app`) | First run: **`157.0 ms`**; Warm median: **`139.3 ms`** (`min 136.1 ms`, `max 140.2 ms`, 3 warm runs) | Measured via `open -a /Applications/Quarry.app <csv>` until `CGWindowListCopyWindowInfo` reported an on-screen layer-0 `Quarry` window (`< 250 ms` warm target met) |

Because full-parity `quarry-gpui` measures `26.39 MiB` on macOS ARM64 (just above the early `25 MB` prototype target), `C10` in `docs/V1_CLAIM_LEDGER.md` is updated to report the exact measured binary sizes and launch latency rather than claiming `<= 25 MB`.

---

## 4. Live macOS Cutover, Rollback, Re-install & OS Integration (`macOS VM 1`)

Environment: `macOS VM 1` (`macOS 27.2.1`, build `26B131`, `aarch64`), VMPal MCP.

1. **Installer Self-Test:** `./scripts/macos-app.sh self-test` passed (`Rollback self-test passed`, `License notice self-test passed`, `17/17` notice tests).
2. **Live Cutover (`quarry-egui` → `quarry-gpui`):**
   - Previous `/Applications/Quarry.app`: signed `9.90 MiB` (`10,351,696` B) `quarry-egui` (`b3afd22`).
   - Ran `./scripts/macos-app.sh install && ./scripts/macos-app.sh verify`: cleanly archived previous bundle to `Quarry-previous.zip` and installed signed `26.39 MiB` (`27,675,376` B) `quarry-gpui` (`QuarryGitRevision = 3523acb172bb12f79ae2e7cc8bb509e4e577fc94`, `CFBundleVersion = 197`, `QuarrySourceStatus = clean`, `codesign --verify --deep --strict` pass).
3. **Live Rollback (`./scripts/macos-app.sh rollback`):**
   - Cleanly restored the verified signed `9.90 MiB` (`10,351,696` B) `quarry-egui` (`b3afd22`) bundle with valid signature, `lsregister` update, and rollback safety backup.
4. **Live Re-install (`./scripts/macos-app.sh install`):**
   - Cleanly reinstalled and verified signed `26.39 MiB` `quarry-gpui` (`3523acb`).
5. **Live Install-Lock Contention Guard:**
   - While `/Applications/Quarry.app` was running (PID `78304`, holding shared `flock` on `/private/tmp/io.github.danchamorro.quarry.501.install.lock`), `./scripts/macos-app.sh install` refused to overwrite the app and exited cleanly with `error: Quit Quarry before installing an update.`.
6. **Live Multi-Window, Finder Hard-Link Routing, About Quarry & `accesskit_macos` (`T2`):**
   - Opened `alpha.csv` and `beta.csv` via `open -a /Applications/Quarry.app`, clicked **Move to Window** so `[a4a]` (`beta.csv · Quarry`) focused in front of `[a3d]` (`alpha.csv · Quarry`), then ran `open -a /Applications/Quarry.app /tmp/quarry-4b3/alpha_hardlink.csv`.
   - Verified single process (`PROCESS_COUNT=1`, `RSS = 100.97 MiB`), `[a3d]` (`alpha.csv · Quarry`) jumped back to front and focused with **zero duplicate tabs**.
   - Opened **Quarry → About Quarry**: displayed `Quarry 0.1.0`, `Build: 197`, `Revision: 3523acb172bb12f79ae2e7cc8bb509e4e577fc94`, `Source status: clean`, `Platform: macos / arm64`, and **Copy Build Details** copied the exact string to the system clipboard (`pbpaste` verified).
   - Live `accesskit_macos` accessibility tree read via `vmpal_tree` in 18–98 ms exposed the native macOS menu bar (`Quarry`, `File`, `Edit`, `Data`, `View`, `Window`), `tab group "Open documents"`, `toolbar "Document toolbar"`, `table "alpha.csv data grid"` with column headers, rows, per-cell `column: value` labels, and the status bar.

---

## 5. Live Unlocked Linux / Omarchy Wayland, Fractional Scaling & AT-SPI (`Omarchy VM 4`)

Environment: `Omarchy VM 4` (`Omarchy 7.3.0-3.1-omarchy`, `aarch64`, Hyprland `0.56.2`, Mesa `26.2.5` Vulkan 1.4, **unlocked desktop session**).

1. **Native Wayland & Omarchy Theme Sync (`C09`, `C14`, `C42`):**
   - Launched `target/release/quarry-gpui` with `~/.config/quarry/theme-preferences` set to `omarchy` (`tokyo-night`).
   - `hyprctl clients -j` confirmed `"class": "dev.quarry.gpui"`, `"xwayland": false` (100% native Wayland), `VmRSS = 315.55 MiB`, `35` open kernel FDs (`< 128`), and `0` open network sockets (`ss -tupn`).
2. **Unlocked Hyprland Fractional Scaling Verification (`C09`):**
   - **`2.0x` (default):** `Virtual-1` (`7680×2160` logical/scaled -> `3816×1030` client, `xwayland: false`), screenshot visually verified crisp `tokyo-night` chrome, tab bar, toolbar, right-aligned `tnum` grid, and status bar.
   - **`1.25x` request:** `hl.monitor(..., scale = 1.25)` (Hyprland snapped to nearest exact divisor `scale = 1.2` on `3840×1992`), `dev.quarry.gpui` cleanly re-tiled to `3176×1610` (`xwayland: false`) with zero crashes or layout breakage.
   - **`1.5x`:** `hl.monitor(..., scale = 1.5)` on `3840×1992`, `dev.quarry.gpui` cleanly re-tiled to `2536×1278` (`xwayland: false`); full-resolution screenshot visually verified crisp `cosmic-text` typography, sharp borders, zero XWayland blur, and intact toolbar/grid/status alignment.
   - Restored monitor cleanly back to `scale = 2`.
3. **Live Unlocked AT-SPI Accessibility Verification (`T2`):**
   - Queried `org.a11y.atspi.Registry` on `unix:path=/run/user/1000/at-spi/bus_0`: confirmed `quarry-gpui` registered with **64** live semantic nodes under `alpha.csv · Quarry` (`Quarry`–`Window` menu items, `Open documents` tab strip with close/move/new-window controls, `Document toolbar` with all 15 controls, and `alpha.csv data grid` with column headers, row summaries, and per-cell `id: 1`, `region: North`, `revenue: 12345.67`, `status: active` labels).

---

## 6. Honest Venue Boundaries & Deferred Items

- **120 Hz hardware frame pacing (`C07`, `C08`):** Virtualized VMs validate correctness, accessibility, fractional scaling, and memory/FD ceilings, not 120 Hz display presentation deadlines. Bare-metal 120 Hz ProMotion / hardware Vulkan frame-deadline tracing remains gated on an owner-approved physical hardware venue (`T18`, Phase 6 / Phase 9).
- **Audible VoiceOver speech & macOS continuous peak-RSS sampling:** Both `macOS VM 1` (`accesskit_macos`) and `Omarchy VM 4` (`accesskit_unix`) verified the live OS accessibility trees; audible VoiceOver speech-caption check and continuous macOS peak-RSS / tab-switch percentile tracing under interactive multi-window churn remain tracked for Phase 6 / Phase 9.
- **Retirement of `apps/quarry-egui`:** `apps/quarry-egui` remains intact in the Cargo workspace until explicit post-cutover owner sign-off to retire it.
