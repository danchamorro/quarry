# Bounded copy validation

## Decision

Ship one selected cell or one selected row for the viewer alpha. A cell copies
its complete decoded UTF-8 value. A row copies every actual field as UTF-8 TSV,
quoting fields with tabs, line breaks, or quotes and excluding the header and
synthetic row number. The serialized payload is capped at 64 MiB, so clipboard
memory does not scale with file size.

Multi-cell ranges, multi-row ranges, and drag selection remain outside this
slice.

## Environment and datasets

- Date: 2026-08-16
- Hardware: Apple M3 Max MacBook Pro, 16 CPU cores, 128 GB RAM
- OS: macOS 26.6.1 (25G76), arm64
- Rust: 1.88.0
- Build: `cargo build -p quarry-egui --release --locked`
- Cache state: unknown; the clipboard exercise did not control the OS file cache
- Small fixture: 64 bytes, SHA-256
  `32e23fbff1d56a861384b866d89488c310453d75eae50fe9920c4984d61d2caa`
- Reference file: `LARGE_FILE.csv`, 12,167,847,982 bytes (11.33 GiB), modified
  2026-08-14 19:00:00 EDT
- Reference rows: 117,168,829 data rows after the header

The small fixture contains a quoted field with an embedded newline and a field
with an embedded tab.

## Results

Computer Use exercised the release build through the macOS accessibility tree.

| Check | Result |
|---|---|
| Select multiline cell | The cell was exposed as a selected toggle and Copy became enabled. |
| Command+C | Pasting into the viewer Find field produced `line one line two`, with its newline normalized by that single-line target. |
| Visible Copy action | Pasting into TextEdit produced `alpha\t"line one\nline two"\t42`, with real tabs and a real newline. |
| Row shape | The header and synthetic row number were excluded; all three actual fields were included. |
| Deep-row navigation | The bounded viewport read after the jump measured 3.347 ms. |
| Deep-row copy | Column 2 copied and pasted back exactly as `BARBARA`. |

The clipboard check changes only the bounded selected field or row. It does not
rescan the source file or retain a file-sized selection.

## Automated coverage

The `quarry-egui` regressions cover:

- complete cell text beyond display truncation;
- TSV escaping for tabs, quotes, and multiline fields;
- empty and invalid UTF-8 fields;
- payload rejection at the configured byte limit;
- Command+C without stealing the shortcut from Quarry text inputs;
- selection preservation while visible and clearing after viewport shrink;
- stable accessibility node identity when a selected record changes screen
  position.

Timing and clipboard integration remain release-build checks rather than CI
thresholds.
