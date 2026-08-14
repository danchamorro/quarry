# Fixtures

Generate large fixtures locally; do not commit them.

```bash
cargo run --release -p quarry-cli --bin quarry -- generate \
  --size 10GB --columns 40 --delimiter , \
  --output fixtures/generated/test-10gb.csv --seed 1
```

The mixed deterministic profile includes quoted delimiters, escaped quotes,
embedded newlines, variable values, and periodic long fields.
