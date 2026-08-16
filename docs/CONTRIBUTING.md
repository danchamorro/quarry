# Contributing to Quarry

Quarry is an open-source, performance-first project. Contributions are welcome
under the dual MIT or Apache-2.0 license.

## Priorities
In the early project, contributions should favor:
1. Correctness
2. Large-file scalability
3. Measured performance
4. Reliability/data safety
5. Maintainability
6. Feature breadth

## Development expectations
Rust code should use `rustfmt`, `clippy`, focused tests, clear error handling, and documented unsafe code. Unsafe code is allowed only when justified by measurement and accompanied by invariants and tests.

## Performance-sensitive changes
A pull request affecting parsing, indexing, caching, search, viewport access, sorting, or export should include relevant benchmark results.

Include:
- Machine/CPU/RAM
- macOS version
- Rust version/toolchain
- Release/debug mode
- Dataset profile and size
- Cold/warm cache status where relevant
- Before/after measurements

Do not submit performance claims based only on tiny files.

## Data correctness
Add regression fixtures/tests for parsing bugs. Important cases include quoted delimiters, escaped quotes, embedded newlines, CRLF/LF, empty fields, empty records, malformed quoting, huge fields, and Unicode boundaries.

## Architecture changes
Material architectural changes should use an ADR under `docs/adr/` describing context, options considered, decision, consequences, and benchmark evidence when performance-related.

## UI contributions
The measured UI bake-off selected egui for the production viewer. UI changes
must keep the engine contract independent, preserve accessibility, and include
focused interaction regressions. [ADR 0003](adr/0003-select-egui-ui.md) records
the decision.

## Scope discipline
Quarry is not trying to become a generic IDE or spreadsheet. Feature proposals should explain how they help the core massive-delimited-file workflow and how they scale beyond RAM.

## Suggested local workflow
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

These commands match the required CI checks and release-build validation.

## Pull requests
Keep changes focused. Explain the problem, design, tradeoffs, tests, and performance impact. Screenshots are useful for UI work; benchmark tables are useful for engine work.

## License
Quarry is dual-licensed under MIT or Apache-2.0. Contributions are accepted
under those same terms.
