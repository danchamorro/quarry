# Contributing to Quarry

Quarry is intended to be an open-source, performance-first project. Contributions are welcome once the repository and license are finalized.

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
Until the UI bake-off is complete, do not assume the final framework. UI prototypes must use the same engine contract so comparisons are meaningful.

## Scope discipline
Quarry is not trying to become a generic IDE or spreadsheet. Feature proposals should explain how they help the core massive-delimited-file workflow and how they scale beyond RAM.

## Suggested local workflow
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo bench
```

Exact commands will evolve as the repository is scaffolded.

## Pull requests
Keep changes focused. Explain the problem, design, tradeoffs, tests, and performance impact. Screenshots are useful for UI work; benchmark tables are useful for engine work.

## License
Quarry is dual-licensed under MIT or Apache-2.0. Contributions are accepted
under those same terms.
