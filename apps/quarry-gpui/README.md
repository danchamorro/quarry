# Quarry GPUI desktop app

The active production desktop frontend for Quarry on macOS and Linux/Omarchy,
built with GPUI Kit `=0.10.0`, `crates/quarry-workbench`, and
`crates/quarry-core` ([ADR 0013](../../docs/adr/0013-adopt-gpui-desktop-frontend.md)).

## Run

```sh
cargo run -p quarry-gpui --release --locked
# Or open a file at startup:
cargo run -p quarry-gpui --release --locked -- /absolute/path/to/file.csv
```

This crate is a member of the root Cargo workspace and shares the root
`Cargo.lock` and pinned Rust `1.100.0` toolchain (`rust-toolchain.toml`).
`scripts/macos-app.sh` packages `apps/quarry-gpui` into `Quarry.app`, and
`packaging/licenses/` tracks its audited third-party notice inventory.

## Validation

```sh
cargo fmt --all -- --check
cargo clippy -p quarry-gpui --all-targets --locked -- -D warnings
cargo test -p quarry-gpui --locked
cargo build -p quarry-gpui --release --locked
```

GPUI Kit and its GPUI implementation are Apache-2.0. Quarry ships with zero
HTTP/TLS/network transport dependencies and zero telemetry (`C01`, `C39`, `C40`).
