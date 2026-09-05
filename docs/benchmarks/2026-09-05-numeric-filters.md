# Numeric filter validation: 2026-09-05

Pre-beta priority 2 adds exact numeric filters on `codex/numeric-filters`, based
on `cd0501384d350e54850711ab7659fb2079f2cbce`. Validation used the uncommitted
feature build. Automated checks, 1 GB validation, and installed-app interaction
checks passed. Implementation commit `d9f47dd` is submitted in
[PR #36](https://github.com/danchamorro/quarry/pull/36), awaiting merge. See the
[priority checklist](../PRE_BETA_CHECKLIST.md#2-numeric-filters) for delivery
status. Priorities 3 and 4 remain planned.

## Behavior

Greater than, Greater than or equal, Less than, Less than or equal, and inclusive
Between share Number sorting's exact decimal key parser. Numeric bounds are
validated before a scan. Blank, missing, and invalid data do not match numeric
rules; blank or invalid bounds and reversed Between bounds reject the query.

Numeric rules preserve the existing grouped logic: inclusion rules within a
column are alternatives, all same-column exclusions apply, and every filtered
column must match. Both bounds inside a Between rule apply. Text matching and
its case setting retain their existing behavior. Filtering, indexed navigation,
and raw-record export evaluate the same query. The
[user guide](../USER_GUIDE.md#numeric-values-and-bounds) documents syntax and
the distinction between separate alternatives and one bounded range.

## Automated validation

The following checks passed:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
```

The initial feature validation passed all 263 workspace tests: 125 core, 103
egui, 25 CLI, nine delimited, and one AppKit test. Seven new tests plus numeric
variants of existing cancellation and record-cap tests cover exact boundaries
and precision, invalid bounds and
data, grouped numeric/text rules, indexed navigation, and byte-exact export.
The desktop interaction test selects the accessible Between control, fills
labelled bounds, applies the filter, verifies rejected drafts preserve the
active rows, and exports the original valid query. Operator-switching coverage
verifies dormant upper bounds do not leak into text or single-bound rules.

Independent UI and CLI integration reviews found no concrete correctness
issues.

CodeRabbit CLI 0.7.5 completed `coderabbit review --agent --uncommitted
--include-untracked` on 2026-09-05 with exit status 0, `review_completed`,
and zero findings across all 14 changed files. File hashes were unchanged
throughout the review. There were no findings to classify as real problems
or optional suggestions in that CLI review.

A later follow-up rejects a following CLI option as a missing `--and between`
upper bound while preserving valid negative bounds. It also enforces the
documented exponent limits in the independent Python validator. All 26 CLI
tests, formatting, strict CLI Clippy, and the CLI release build passed after
this follow-up. The extracted validator's exponent-limit assertions and
additional syntax checks passed; all 24 generated amount values retained their
previous classifications, so the recorded benchmark results remain unchanged.
These follow-up changes were checked separately and are outside the original
CodeRabbit CLI review's scope. Further review fixes preserve the rule index in
empty Contains errors and assert the documented export count, size, and hash.
All 264 workspace tests, formatting, and strict workspace Clippy passed. The
updated verifier passed on the retained 1 GB workload, and each new constant
assertion rejected a deliberately incorrect value. The desktop code is unchanged.

## Environment and scope

- Apple M3 Max, 137,438,953,472 bytes RAM (128 GiB), macOS 26.6.2 (25G83).
- rustc 1.88.0 (6b00bc388 2025-06-23), release profile, locked dependencies.
- Baseline archived from `cd0501384d350e54850711ab7659fb2079f2cbce` and built separately. Candidate is the working `codex/numeric-filters` change based on that commit.
- Input was fully read before measurement. All results are sequential warm filesystem-cache runs. Background system load was not controlled, and a brief build may have overlapped the measurement window. No cold-cache or larger-than-RAM claim.

## Workload

1,000,000,155 bytes, 5,179,437 data records, four columns (`row_id,amount,state,note`), UTF-8 BOM, CRLF, periodic embedded LF/comma/escaped quotes/Unicode in the note, periodic missing columns, and a final matching record without a line terminator. Amounts cycle through signed and padded decimals, scientific notation, exponents outside floating-point range, adjacent high-precision decimal values, blanks, NaN/inf, grouping/currency/date strings, and malformed exponents. The independent validator classified 1,729,940 records as invalid or missing numeric values.

- Source SHA-256 before and after: `b93bdd33d096f195105ced2eb07155d4ac880d7bcb15e2c221d30ace085c2126`.
- Between uses amount `>= -100` and `<= 250`, plus state text equals `TX` on the other column.
- Exact expected export: 1,724,748 records, 334,036,832 bytes, SHA-256 `15ae1c8da605746863e3e3982c4ab6266355e64d4dd8c81c0cc21ff63595ccc9`.
- The validator checked the BOM/header, every original raw record, source order, final missing line terminator, absence of unexpected output bytes, source hash, all filter counts, and both cancellation outcomes.

## Measurements

Filter/export elapsed times are the worker timings printed by the CLI. Peak RSS is reported by `getrusage`, cross-checked by `/usr/bin/time -l`.

| Workload | Matches / published rows | Elapsed | Index memory | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| Numeric `gt 250` | 646,780 | 0.932 s | 7.40 MiB | 24.27 MiB |
| Numeric `gte 250` | 1,077,967 | 0.941 s | 6.17 MiB | 23.02 MiB |
| Numeric `lt -100` | 431,188 | 0.911 s | 9.87 MiB | 24.58 MiB |
| Numeric `lte -100` | 862,375 | 0.953 s | 9.87 MiB | 26.73 MiB |
| Between plus text | 1,724,748 | 0.956 s | 9.87 MiB | 27.77 MiB |
| Between plus text export | 1,724,748 | 1.170 s | No filter index | 4.20 MiB |

The CLI also read 300 bounded sample rows per completed filter scan. Index compaction and exact filtered reads with a deliberately tiny checkpoint budget are covered by the new core regression `numeric_grouped_filters_share_bounded_scan_range_read_and_raw_export_results`. The observed RSS stayed far below the 953.67 MiB source size; this single workload is evidence of bounded streaming operation, not a universal peak-memory bound.

Existing text filter baseline comparison (`state equals TX`, 3,449,494 matches):

| Build | Run 1 | Run 2 | Run 3 | Median | Peak RSS range |
| --- | ---: | ---: | ---: | ---: | ---: |
| Baseline | 0.678 s | 0.682 s | 0.683 s | 0.682 s | 26.61 to 27.67 MiB |
| Candidate | 0.710 s | 0.682 s | 0.682 s | 0.682 s | 26.61 to 28.66 MiB |

There was no median text-filter slowdown in these three paired warm runs. The first candidate invocation measured 1.235 s wall time and a 0.710 s filter scan; later candidate wall times were 0.693 s. The cause of the additional wall time was not established. This is a short local comparison, not a claim of equivalent performance on all datasets.

## Cancellation and file safety

The requested threshold was 100,000,000 bytes in both runs. Progress publishes in chunks, so the observed request points were later than the threshold.

| Job | Observed request point | Final bytes scanned | Poll-inclusive latency | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| Numeric filter | 101,711,872 | 102,760,448 | 1.598 ms | 12.94 MiB |
| Numeric filtered export | 100,663,296 | 101,711,872 | 1.272 ms | 4.22 MiB |

Both outcomes were cancelled before EOF. The partial filter had 176,684 matches and a 4.04 MiB checkpoint index and remained readable through the CLI sample path. The cancelled export published no destination, left no `.quarry*` temporary artifact in its directory, and source SHA-256 remained unchanged.

## Installed-app validation

`./scripts/macos-app.sh install` and `./scripts/macos-app.sh verify` passed,
with the prior valid app retained in the normal rollback archive. The installed
`QuarryGitRevision` is `cd0501384d350e54850711ab7659fb2079f2cbce` and
`QuarrySourceStatus` is `dirty`, identifying this local uncommitted feature
build. No bundles were published.

A disposable 209-byte, nine-row CSV verified the native installed workflow:

1. Open the fixture through the file picker, then open Filters and confirm all
   five numeric operators are available.
2. Choose Between, enter lower bound `100`, and verify upper bound `99` is
   rejected with the reversed-bound explanation. Enter `1e3` as the upper bound.
3. Add a rule on status using Contains `KEEP` with Match case off. Applying
   shows source data rows 2, 3, and 7, including both endpoints and a padded
   scientific number with a quoted multiline note.
4. Change the draft upper bound to `NaN`. Apply filters stays disabled and
   the active query and three matching rows remain unchanged.
5. Export Filtered Rows writes the valid active query, not the invalid draft.
   Verify all 87 output bytes and confirm the source remains unchanged.
6. Clear the filter to restore all nine rows. Close safely and reopen empty.

The exported bytes were:

```python
b'amount,status,note\r\n100,keep,lower\r\n1e3,keep,upper\r\n 1.00e2 ,keep,"line one\nline two"\r\n'
```

Source SHA-256 before and after was
`152507419ace03fe146ad67eb82689c0f3b2a292418c0a5c7e885220e2c6d4d6`.
The exported SHA-256 was
`3a9fe0e5e647dd6e2b23acd2498d86446f7d8549785cb9bed8a052db264e7352`.

## Reproduction

Use Python 3.11 or later for the standard-library validator. Run from the
repository root. Create a fresh artifact directory, then save the
three scripts below into it as `generate.py`, `run.py`, and `verify.py`. Each
fixture and export uses an unused destination.

```sh
# From the repository root, set up a fresh artifact directory:
numeric_validation_dir="$(mktemp -d /private/tmp/quarry-numeric-filter-XXXXXX)"
mkdir "$numeric_validation_dir/baseline"
# Save the three scripts below in "$numeric_validation_dir" before running them.
git archive cd0501384d350e54850711ab7659fb2079f2cbce | tar -x -C "$numeric_validation_dir/baseline"
cargo build --manifest-path "$numeric_validation_dir/baseline/Cargo.toml" -p quarry-cli --release --locked
cargo build -p quarry-cli --release --locked
python3 "$numeric_validation_dir/generate.py"
python3 "$numeric_validation_dir/run.py"
python3 "$numeric_validation_dir/verify.py"
```

Representative commands (the runner repeats baseline/candidate text scans three times):

```sh
/usr/bin/time -l target/release/quarry-bench filter "$numeric_validation_dir/numbers.csv" --column 2 --operator between --value -100 --upper-bound 250 --and 3 equals TX --cache-state warm
/usr/bin/time -l target/release/quarry-bench export "$numeric_validation_dir/numbers.csv" --output "$numeric_validation_dir/filtered.csv" --column 2 --operator between --value -100 --upper-bound 250 --and 3 equals TX --cache-state warm
# Add --cancel-after-bytes 100000000 to either command and use a distinct output for export.
```

The exact invocation list and exit codes are saved in `commands.json`; source metadata is in `dataset.json`, independent results in `independent.json`. Full scripts follow so the fixture and oracle remain reproducible if temporary artifacts are removed.

### generate.py

```python
from pathlib import Path
import csv
import hashlib
import io
import json

root = Path(__file__).parent
path = root / 'numbers.csv'
amounts = [
    '-100.01', '-100', ' -00100.000 ', '-.5', '-0', '+0.00', '.5', '+001.2500',
    '2.5e2', '250.000', '250.00000000000000000001', '1000000000000000000000000000000',
    '-1000000000000000000000000000000', '1e1000000', '-1e-1000000', '', '   ',
    'NaN', 'inf', '1,000', '$42', '2026-09-05', '1e', '123.45678901234567890123456789',
]
written = 0
row_id = 0
with path.open('xb', buffering=8 * 1024 * 1024) as out:
    header = b'\xef\xbb\xbfrow_id,amount,state,note\r\n'
    out.write(header)
    written = len(header)
    buffer = io.StringIO(newline='')
    writer = csv.writer(buffer, lineterminator='\r\n')
    while written < 1_000_000_000:
        row = [str(row_id), amounts[row_id % len(amounts)], 'TX' if row_id % 3 else 'CA', 'x' * 170]
        if row_id % 211 == 0:
            row[3] += '\nline two, "quoted" café'
        if row_id > 0 and row_id % 997 == 0:
            row = [str(row_id)]
        writer.writerow(row)
        data = buffer.getvalue().encode('utf-8')
        buffer.seek(0)
        buffer.truncate(0)
        out.write(data)
        written += len(data)
        row_id += 1
    tail = f'{row_id},123.45,TX,no final newline'.encode()
    out.write(tail)
    written += len(tail)
    row_id += 1
with path.open('rb') as source:
    sha = hashlib.file_digest(source, 'sha256').hexdigest()
result = {'bytes': written, 'data_rows': row_id, 'sha256': sha, 'amount_cycle': amounts}
(root / 'dataset.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result, indent=2))
```

### run.py

```python
from pathlib import Path
import json
import shutil
import subprocess
import time

root = Path(__file__).parent
repo = Path.cwd()  # Run this script from the repository root.
source = root / 'numbers.csv'
candidate = root / 'candidate-quarry-bench'
shutil.copy2(repo / 'target/release/quarry-bench', candidate)
baseline = root / 'baseline/target/release/quarry-bench'
records = []
def run(name, binary, args):
    command = ['/usr/bin/time', '-l', str(binary), *map(str, args)]
    started = time.monotonic()
    result = subprocess.run(command, capture_output=True, text=True)
    elapsed = time.monotonic() - started
    (root / (name + '.log')).write_text(result.stdout + result.stderr)
    record = {'name': name, 'command': command, 'exit_code': result.returncode, 'wall_seconds': elapsed}
    records.append(record)
    (root / 'commands.json').write_text(json.dumps(records, indent=2) + '\n')
    print(f'{name}: exit={result.returncode} wall={elapsed:.3f}s', flush=True)
    if result.returncode:
        raise SystemExit(result.stdout + result.stderr)
# Read the entire input once to ensure that all measured passes start warm.
with source.open('rb') as stream:
    while stream.read(8 * 1024 * 1024):
        pass
text_args = ['filter', source, '--column', '3', '--operator', 'equals', '--value', 'TX', '--cache-state', 'warm']
for repetition in range(1, 4):
    run(f'baseline-text-{repetition}', baseline, text_args)
    run(f'candidate-text-{repetition}', candidate, text_args)
for operator, bound in [('gt', '250'), ('gte', '250'), ('lt', '-100'), ('lte', '-100')]:
    run(f'numeric-{operator}', candidate, ['filter', source, '--column', '2', '--operator', operator, '--value', bound, '--cache-state', 'warm'])
numeric_args = ['--column', '2', '--operator', 'between', '--value', '-100', '--upper-bound', '250', '--and', '3', 'equals', 'TX', '--cache-state', 'warm']
run('numeric-between', candidate, ['filter', source, *numeric_args])
run('numeric-export', candidate, ['export', source, '--output', root / 'filtered.csv', *numeric_args])
run('numeric-cancel', candidate, ['filter', source, *numeric_args, '--cancel-after-bytes', '100000000'])
run('numeric-export-cancel', candidate, ['export', source, '--output', root / 'cancelled.csv', *numeric_args, '--cancel-after-bytes', '100000000'])
assert not (root / 'cancelled.csv').exists()
assert not list(root.glob('.quarry-export-*')), 'Export temporary file remains'
```

### verify.py

The validator also checks exponent limits in memory without changing the
recorded benchmark dataset.

```python
from pathlib import Path
from decimal import Decimal
import csv
import hashlib
import json
import re

root = Path(__file__).parent
# CSV decoding and exact Decimal comparisons are independent of Quarry's parser.
number = re.compile(r'[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE]([+-]?[0-9]+))?\Z')
cache = {}
def numeric(value):
    value = value.strip(' \t\r\n\v\f')
    if value not in cache:
        match = number.fullmatch(value)
        valid = match is not None and -1_000_000 <= int(match.group(1) or '0') <= 1_000_000
        cache[value] = Decimal(value) if valid else None
    return cache[value]
for exponent in [-1_000_000, 1_000_000]:
    value = f'1e{exponent}'
    assert numeric(value) == Decimal(value), value
for exponent in [-1_000_001, 1_000_001]:
    value = f'1e{exponent}'
    assert numeric(value) is None, value
class RecordingLines:
    def __init__(self, stream):
        self.stream = stream
        self.raw = []
    def __iter__(self):
        return self
    def __next__(self):
        line = next(self.stream)
        self.raw.append(line)
        return line.decode('utf-8')
    def take(self):
        raw = b''.join(self.raw)
        self.raw.clear()
        return raw
source_hash = hashlib.sha256()
output_hash = hashlib.sha256()
counts = dict.fromkeys(['gt', 'gte', 'lt', 'lte', 'between', 'text', 'invalid'], 0)
rows = 0
output_bytes = 0
with (root / 'numbers.csv').open('rb', buffering=8*1024*1024) as source, (root / 'filtered.csv').open('rb', buffering=8*1024*1024) as output:
    lines = RecordingLines(source)
    reader = csv.reader(lines, strict=True)
    assert next(reader) == ['\ufeffrow_id', 'amount', 'state', 'note']
    header = lines.take()
    assert output.read(len(header)) == header
    source_hash.update(header)
    output_hash.update(header)
    output_bytes += len(header)
    for row in reader:
        raw = lines.take()
        source_hash.update(raw)
        assert int(row[0]) == rows
        rows += 1
        value = numeric(row[1]) if len(row) > 1 else None
        is_tx = len(row) > 2 and row[2] == 'TX'
        counts['text'] += is_tx
        if value is None:
            counts['invalid'] += 1
            continue
        counts['gt'] += value > Decimal('250')
        counts['gte'] += value >= Decimal('250')
        counts['lt'] += value < Decimal('-100')
        counts['lte'] += value <= Decimal('-100')
        if Decimal('-100') <= value <= Decimal('250') and is_tx:
            assert output.read(len(raw)) == raw, f'raw exported row differs: {row[0]}'
            output_hash.update(raw)
            output_bytes += len(raw)
            counts['between'] += 1
    assert output.read(1) == b'', 'Unexpected trailing output bytes'
metadata = json.loads((root / 'dataset.json').read_text())
assert rows == metadata['data_rows']
assert source_hash.hexdigest() == metadata['sha256'], 'Source changed'
assert (root / 'numbers.csv').stat().st_size == metadata['bytes']
assert output_bytes == (root / 'filtered.csv').stat().st_size
for operator in ['gt', 'gte', 'lt', 'lte', 'between']:
    log = (root / f'numeric-{operator}.log').read_text()
    actual = int(re.search(r'^Matches found: (\d+)$', log, re.M).group(1))
    assert actual == counts[operator], (operator, actual, counts[operator])
for build in ['baseline', 'candidate']:
    for repetition in range(1, 4):
        log = (root / f'{build}-text-{repetition}.log').read_text()
        actual = int(re.search(r'^Matches found: (\d+)$', log, re.M).group(1))
        assert actual == counts['text'], (build, actual, counts['text'])
export = (root / 'numeric-export.log').read_text()
assert int(re.search(r'^Published rows: (\d+)$', export, re.M).group(1)) == counts['between'] == 1_724_748
assert output_bytes == 334_036_832
assert output_hash.hexdigest() == '15ae1c8da605746863e3e3982c4ab6266355e64d4dd8c81c0cc21ff63595ccc9'
for name in ['numeric-cancel', 'numeric-export-cancel']:
    log = (root / f'{name}.log').read_text()
    assert 'Outcome: cancelled' in log
    scanned = int(re.search(r'^Bytes scanned: .*?\((\d+) bytes\)', log, re.M).group(1))
    assert 100000000 <= scanned < metadata['bytes']
assert not (root / 'cancelled.csv').exists()
assert not list(root.glob('.quarry*')), 'Temporary export artifact remains'
result = {'data_rows': rows, 'matches': counts, 'source_sha256_before_and_after': source_hash.hexdigest(), 'filtered_bytes': output_bytes, 'filtered_sha256': output_hash.hexdigest(), 'checks': 'Every exported raw record byte-equal and in source order; all five filter counts match Decimal; text counts match; source unchanged; cancellation partial and destination/temp absent.'}
(root / 'independent.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result, indent=2))
```
