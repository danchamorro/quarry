# Contributing to Quarry

Thank you for your interest in Quarry.

Quarry is a free, open-source (`MIT OR Apache-2.0`), local-first desktop and CLI editor for multi-gigabyte CSV, TSV, and delimited text files on **macOS** and **Linux / Omarchy**. There is no Contributor License Agreement (CLA): by submitting a pull request, you agree to license your contributions under the repository's existing `MIT OR Apache-2.0` terms.

Before making substantial changes, read:

- [`README.md`](../README.md)
- [`docs/PRD.md`](PRD.md)
- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md)
- [`docs/DESIGN.md`](DESIGN.md)
- [`docs/ROADMAP.md`](ROADMAP.md)

## Repository Layout

- `crates/quarry-delimited` — quote-aware streaming delimited scanner and runtime-dispatched 64-byte SIMD structural scanner (Apple Silicon NEON, x86_64 AVX2, scalar fallback).
- `crates/quarry-core` — streaming engine: parallel positional I/O, persistent sidecar index cache, search, filter, sort, duplicates, join, combine, section windows, guarded splice-save, inspection/recovery, and atomic saves.
- `crates/quarry-workbench` — frontend-neutral desktop workbench state: document/tab lifecycle, multi-window dormancy scheduler, undo/redo, bookmarks, and semantic Light/Dark/Omarchy theme palettes.
- `apps/quarry-gpui` — active GPU-rendered Rust desktop application for macOS and Linux/Omarchy ([ADR 0013](adr/0013-adopt-gpui-desktop-frontend.md)).
- `apps/quarry-cli` — headless CLI (`quarry-cli`) for scripting, validation, and benchmarking.
- `apps/quarry-appkit` — frozen macOS AppKit comparator retained for historical reference.
- `docs/` — architecture, design system, user guide, Linux/Omarchy and macOS packaging guides, ADRs, claim ledger, and benchmark reports.
- `scripts/` — macOS app packaging/verification, third-party notice generation, and benchmark runners.
- `packaging/` — macOS `Info.plist` and audited third-party license notices.

## Development Workflow

Quarry pins its Rust toolchain in [`rust-toolchain.toml`](../rust-toolchain.toml) (currently **Rust `1.100.0`**).

### Validation on macOS

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
./scripts/generate-notices.sh --check
```

To build and verify the local macOS app bundle (or run `./scripts/macos-app.sh install` followed by `./scripts/macos-app.sh verify` to install and verify `/Applications/Quarry.app`):

```bash
./scripts/macos-app.sh package
./scripts/macos-app.sh verify target/package/Quarry.app
```

### Validation on Linux / Omarchy

`apps/quarry-appkit` is macOS-only, so scope Linux Clippy and test commands to the cross-platform crates:

```bash
cargo fmt --all -- --check
cargo clippy -p quarry-delimited -p quarry-core -p quarry-workbench -p quarry-gpui -p quarry-cli --all-targets --locked -- -D warnings
cargo test -p quarry-delimited -p quarry-core -p quarry-workbench -p quarry-gpui -p quarry-cli --locked
cargo build --release -p quarry-gpui --locked
```

See [`docs/LINUX_AND_OMARCHY.md`](LINUX_AND_OMARCHY.md) for required Linux system libraries and Wayland/Hyprland testing details.

## Engineering & Design Rules

- **Bounded memory rule:** Never add a feature that loads an entire CSV file into memory just to make it work. Every operation must scale with bounded RAM, explicit disk-spill preflight when needed, and cancellation cleanup.
- **No `mmap` on user source files:** Use the shared positional `pread`/`seek_read` I/O layer so external truncation produces a clean error instead of `SIGBUS` ([ADR 0001](adr/0001-initial-engine.md)).
- **Safe writes & source guards:** Preserve atomic staging, no-clobber publication, and source-modification checks ([ADR 0005](adr/0005-temporary-storage.md), [ADR 0015](adr/0015-guarded-section-splice-save.md)).
- **Zero network / zero telemetry:** Do not introduce HTTP, TLS, telemetry, update-check, or network-transport dependencies into shipping crates.
- **Frontend & visual consistency:** New UI changes belong in `apps/quarry-gpui` and `crates/quarry-workbench` following [`docs/DESIGN.md`](DESIGN.md). Support all built-in palettes (**Light**, **Dark**, **System**, and **Omarchy**) using the semantic tokens in `crates/quarry-workbench/src/theme.rs`—never hardcode ad-hoc colors—and keep macOS AccessKit and Linux AT-SPI labels accurate.
- **Third-party notices:** Whenever workspace dependencies or bundled assets change, audit licenses and check `./scripts/generate-notices.sh --check` per [`packaging/licenses/AUDIT.md`](../packaging/licenses/AUDIT.md).
- **Changelog:** Update [`CHANGELOG.md`](../CHANGELOG.md) in the same PR with concise, plain-English user-facing bullets under the merge date.

## Pull Requests

- Use Conventional Commit titles in plain language (for example, `fix(ui): keep tab labels readable in narrow windows`).
- Explain the problem and fix concisely in the PR description.
- Include before/after screenshots (or a short clip for motion/timing) for UI changes, using synthetic data only.
- Include unit or integration tests covering correctness, edge cases, cancellation, and error handling.
- Keep `main` green: formatting, strict Clippy, tests, and notice checks must pass before merging.
