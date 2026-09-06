# Find and remove duplicates validation: 2026-09-05

Large-file CLI validation passed for pre-beta priority 3 on
`codex/find-remove-duplicates`, based on
`8ab518566630920a1cb00d4f646ebf590dc3d9d2`. Every retained output record matched
an independent standard-library Python oracle. The installed-app workflow and
integrated check results are tracked in the
[priority checklist](../PRE_BETA_CHECKLIST.md#3-find-and-remove-duplicates).

## Behavior under validation

Selected decoded columns determine a duplicate key. Matching defaults to
ASCII-insensitive comparison; Match case compares exact decoded bytes. Missing
fields and empty fields compare equally, while whitespace remains significant.
The first occurrence in the current document order is retained, followed by
other retained rows in their original relative order. Duplicate count means
extra occurrences to remove, excluding the retained first occurrence. The
header stays fixed.

The engine prepares a private candidate while finding duplicates. The desktop
shows the duplicate count and requires explicit removal before adopting that
candidate. Count-only CLI analysis removes its completed private candidate;
`--output` retains the result at an unused destination.

## Environment and scope

- Apple M3 Max, 137,438,953,472 bytes RAM (128 GiB).
- macOS 26.6.2 (25G83), rustc 1.88.0 (6b00bc388 2025-06-23).
- Release profile, locked dependencies, local uncommitted feature build.
- Copied benchmark binary SHA-256:
  `8c599b86c1d7cc59915424e3e763443209456f59ba8f3cce45b814afec90c6b7`.
- Input was fully read before measurement. The six reported passes ran
  sequentially against a warm filesystem cache. Background system load was
  not controlled, and a brief focused core test may have overlapped.
- An earlier six-pass suite was excluded after a known overlap with CLI tests.
  Its artifacts remain under `initial-possible-overlap` in the local validation
  directory. Only the subsequent suite is reported below.
- Duplicate analysis/removal did not exist before this change, so there is no
  equivalent before-change operation to time. These are single local
  observations, with no speedup, cold-cache, or larger-than-RAM claim.

## Deterministic workload

The source contains 1,000,000,048 bytes and 3,666,416 data records. Its columns
are `name,row_id,region,note`. The main key uses columns 1 and 3; the unselected
row ID and note deliberately vary across repetitions. Keys follow a permuted
cycle of 200,003 values.

The fixture includes a UTF-8 BOM, CRLF, alternative CSV quoting, decoded keys
with embedded newlines, commas and escaped quotes, ASCII case variants,
non-ASCII `Å` and `å`, missing fields, empty fields, significant spaces,
different tuples that would collide if joined with `|`, repeated identical
records, and a unique last record without a line terminator.

Source SHA-256 before and after all operations:
`273b272669610a80922e19def23e9c85b6de5fc0a1e2c3ce46f881bba50adcb9`.

## Measured results

All six commands exited successfully. Job time is the CLI's
`Duplicate wall time`; peak RSS is cross-checked with `/usr/bin/time -l`.
Temporary disk is the engine's tracked payload bytes for private sort runs and
staged output, excluding source bytes and filesystem/metadata overhead.

| Operation | Extra occurrences | Retained rows | Output bytes | Job time | Peak RSS | Peak temporary bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Columns 1,3, count only | 3,466,210 | 200,206 | 53,724,244, candidate deleted | 4.771 s | 52.12 MiB | 1,967,567,111 |
| Columns 1,3, default matching | 3,466,210 | 200,206 | 53,724,244 | 5.046 s | 38.78 MiB | 1,967,567,111 |
| Columns 1,3, Match case | 3,266,010 | 400,406 | 109,141,949 | 4.849 s | 38.92 MiB | 1,967,567,111 |
| Column 3 only, default matching | 3,666,408 | 8 | 1,345 | 3.164 s | 29.38 MiB | 1,663,939,337 |

The default-key workloads created 93 sorted runs, Match case created 97, and
the region-only workload created 72. Each completed three merge passes.
The implementation uses 16 MiB run buffers, a maximum fan-in of 32 reduced for
wide keys, and 64 MiB record/key limits. A duplicate key includes an 8-byte
length prefix for each selected field, so its limit includes that framing.
The observed 29.38 to 52.12 MiB peak RSS is evidence of bounded processing for
this 953.67 MiB input, not a universal peak-memory bound.

The independent oracle parses source CSV, forms tuples of decoded UTF-8 bytes,
and tracks the first occurrence in source order. ASCII folding uses an explicit
byte translation table, with no Unicode case folding or whitespace trimming.
For every retained key, the corresponding output bytes must exactly match the
original raw source record. It also rejects extra output bytes, verifies
reported CLI counts, checks fixed fixture/output constants, and confirms that
the source hash is unchanged.

| Output | SHA-256 |
| --- | --- |
| Default columns 1,3 | `8c02eb2268452e68c5e0c5c2fd849c531acf0eec2d16bd381fa5638454fb9014` |
| Match case columns 1,3 | `ba736bfebe3211ecadbe356746a2465908766238592e8b49d9ba4fd00a9a1931` |
| Default column 3 | `8214860c61cf4d55cf5911ec8a0dc16fcb97915277f53e2188d43f2717dbaaf2` |

## Cancellation and cleanup

Both cancellation runs requested cancellation after the scanner reached
100,000,000 source bytes. Chunked progress reporting accounts for the later
observed request points. These runs exercise cancellation during initial run
creation, with eight private sorted runs already written.

| Job | Observed request point | Final bytes scanned | Cancellation latency | Peak RSS | Peak temporary bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Count only | 100,663,296 | 101,711,872 | 1.830 ms | 31.31 MiB | 134,216,366 |
| Retained output | 101,711,872 | 102,760,448 | 1.864 ms | 31.30 MiB | 134,216,366 |

Both returned `cancelled` before EOF and published no destination. The
analysis-only candidate directories named by the CLI were absent afterward,
and the output directory contained no `.quarry-sort-*` or `.quarry-export-*`
artifacts. The cancelled output path did not exist. Source SHA-256 remained
unchanged. Full analysis-only completion also removed its prepared candidate.

## Integrated and installed-app validation

All 276 workspace tests passed: 132 core, 106 egui, 28 CLI, nine delimited,
and one AppKit. The new coverage includes seven core duplicate regressions,
three desktop duplicate regressions, and two CLI duplicate regressions.
Formatting, strict all-targets/all-features Clippy, the workspace release
build, and `git diff --check` passed.

Focused checks cover effective sparse values, header edits, exact retained
records, selected-column and case semantics, forced multipass merging, empty
and headerless files, invalid requests, source changes, destination conflicts,
cancellation and abandoned-job cleanup. Desktop checks also cover preview
accessibility, preserving history after a failed candidate reopen, blocking
Save/Save As during a structural operation, Undo/Redo, Save As, and Discard.

The local installer preserved its verified rollback archive, then installed
and verified `/Applications/Quarry.app`. This is an uncommitted feature build:
`QuarryGitRevision` is `8ab518566630920a1cb00d4f646ebf590dc3d9d2` and
`QuarrySourceStatus` is `dirty`. It is not a clean merged release candidate.

Native accessibility actions and a visual layout check passed on a disposable
83-byte BOM/CRLF fixture with four data rows and a quoted multiline value:

1. Changed the third row's key from `b` to `a` without saving.
2. Selected `key` and `status` and opened **Find Duplicates…**. The dialog
   exposed both column names, **Match case**, matching semantics, and controls.
3. Found two extra rows and two retained rows. The review showed both counts
   and the keep-first explanation while editing was blocked.
4. Cancelled the preview. All four rows and the unsaved edit remained.
5. Repeated the search and explicitly removed the two extra rows. Only the
   first `a,keep` row and the later `a,skip` row remained, with the multiline
   field and header intact.
6. Undo restored all four rows and the earlier unsaved edit; Redo restored the
   two retained rows.
7. Save As wrote a new file and reopened it clean. Every output byte matched
   the expected 55-byte BOM/CRLF file. Source bytes were unchanged.

Native fixture: `/private/tmp/quarry-duplicates-ui-gq2k1ufo/duplicates.csv`.
Source SHA-256:
`e351e72fb791182ac21c4f440ab3caaabf327d3fc666977f061d0a49398a602a`.
Saved output SHA-256:
`ad65ff4638022c69de84c97322a1f07d56038d0aa20f2a7748e01cac603eb094`.

An independent agent reviewed the actual GUI lifecycle and shared core sort
changes. The shared Save guard was added after a regression exposed it; no
additional concrete issue was found. This was source review, not a CodeRabbit
CLI run. The feature has not been committed, pushed, or submitted as a PR.

## Reproduction

Use Python 3.11 or later. Run from the repository root with the feature
checkout, save the three scripts below into a fresh artifact directory, then
run:

```sh
duplicate_validation_dir="$(mktemp -d /private/tmp/quarry-duplicate-validation-XXXXXX)"
# Save generate.py, run.py, and verify.py below in this directory.
cargo build -p quarry-cli --release --locked
python3 "$duplicate_validation_dir/generate.py"
python3 "$duplicate_validation_dir/run.py"
python3 "$duplicate_validation_dir/verify.py"
```

The timing utility needs access to the system CPU/RSS metrics. If it fails,
report that failure and rerun with appropriate read-only metrics access rather
than treating its output as a completed benchmark. The runner records every
invocation and exit code in `commands.json`, source details in `dataset.json`,
and oracle results in `independent.json`. Fixture and output files require
unused destinations. The original local evidence is under
`/private/tmp/quarry-duplicate-validation-2026-09-05`.

Representative commands:

```sh
target/release/quarry-bench duplicates "$duplicate_validation_dir/duplicates.csv" --columns 1,3 --header first-row --cache-state warm
target/release/quarry-bench duplicates "$duplicate_validation_dir/duplicates.csv" --columns 1,3 --output "$duplicate_validation_dir/insensitive.csv" --header first-row --cache-state warm
target/release/quarry-bench duplicates "$duplicate_validation_dir/duplicates.csv" --columns 1,3 --match-case --output "$duplicate_validation_dir/sensitive.csv" --header first-row --cache-state warm
```

The verification script uses a Python set as an independent reference, so its
own RAM consumption is outside the measured Quarry process.

### generate.py

```python
from pathlib import Path
import csv
import hashlib
import io
import json

root = Path(__file__).parent
path = root / 'duplicates.csv'
written = 0
rows = 0
cycle = 200_003
with path.open('xb', buffering=8 * 1024 * 1024) as out:
    header = b'\xef\xbb\xbfname,row_id,region,note\r\n'
    out.write(header)
    written += len(header)
    buffer = io.StringIO(newline='')
    writers = [csv.writer(buffer, lineterminator='\r\n', quoting=mode)
               for mode in (csv.QUOTE_MINIMAL, csv.QUOTE_ALL)]
    while written < 1_000_000_000:
        key_id = (rows * 104729) % cycle
        occurrence = rows // cycle
        name = f'key-{key_id:06d}-' + ('x' * 64)
        region = ['US', 'EU', 'APAC'][key_id % 3]
        if occurrence % 2:
            name = name.upper()
            region = region.lower()
        if key_id % 211 == 0:
            name += '\nline, "quoted"'
        if key_id % 997 == 0:
            name = ('Å' if occurrence % 3 else 'å') + name
        if key_id == 1:
            name, region = 'a|b', 'c'
        elif key_id == 2:
            name, region = 'a', 'b|c'
        elif key_id == 3:
            name, region = '', ''
        elif key_id == 4:
            name, region = ' ', ''
        note = 'n' * 180
        if rows % 307 == 0:
            note += '\nsecond line, "quoted" café'
        row = [name, str(rows), region, note]
        if rows > 0 and rows % 991 == 0:
            row = ['']  # Both selected columns empty, including missing region.
        elif rows > 0 and rows % 991 == 1:
            row = ['', str(rows), '', note]
        if rows % 1237 in (1, 2):
            row = ['Identical', 'same id', 'X', 'same record']
        writers[(rows // cycle) % 2].writerow(row)
        data = buffer.getvalue().encode('utf-8')
        buffer.seek(0)
        buffer.truncate(0)
        out.write(data)
        written += len(data)
        rows += 1
    # A unique matching key exercises preservation of the final missing terminator.
    tail = b'final-unique,no numeric id,ZZ,no final newline'
    out.write(tail)
    written += len(tail)
    rows += 1
with path.open('rb') as source:
    sha = hashlib.file_digest(source, 'sha256').hexdigest()
result = {'bytes': written, 'data_rows': rows, 'sha256': sha,
          'selected_columns_one_based': [1, 3], 'key_cycle': cycle}
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
repo = Path.cwd()
source = root / 'duplicates.csv'
binary = root / 'candidate-quarry-bench'
shutil.copy2(repo / 'target/release/quarry-bench', binary)
records = []
def run(name, args):
    command = ['/usr/bin/time', '-l', str(binary), 'duplicates', str(source),
               '--header', 'first-row', '--cache-state', 'warm', *map(str, args)]
    started = time.monotonic()
    result = subprocess.run(command, capture_output=True, text=True)
    elapsed = time.monotonic() - started
    (root / (name + '.log')).write_text(result.stdout + result.stderr)
    record = {'name': name, 'command': command, 'exit_code': result.returncode,
              'wall_seconds': elapsed}
    records.append(record)
    (root / 'commands.json').write_text(json.dumps(records, indent=2) + '\n')
    print(f'{name}: exit={result.returncode} wall={elapsed:.3f}s', flush=True)
    if result.returncode:
        raise SystemExit(result.stdout + result.stderr)
with source.open('rb') as stream:
    while stream.read(8 * 1024 * 1024):
        pass
run('count-only', ['--columns', '1,3'])
run('insensitive', ['--columns', '1,3', '--output', root / 'insensitive.csv'])
run('sensitive', ['--columns', '1,3', '--match-case', '--output', root / 'sensitive.csv'])
run('region', ['--columns', '3', '--output', root / 'region.csv'])
run('cancel-count', ['--columns', '1,3', '--cancel-after-bytes', '100000000'])
run('cancel-output', ['--columns', '1,3', '--output', root / 'cancelled.csv',
                      '--cancel-after-bytes', '100000000'])
assert not (root / 'cancelled.csv').exists(), 'Cancelled output was published'
```

### verify.py

```python
from pathlib import Path
import csv
import hashlib
import json
import re

root = Path(__file__).parent
ascii_fold = bytes.maketrans(b'ABCDEFGHIJKLMNOPQRSTUVWXYZ', b'abcdefghijklmnopqrstuvwxyz')
cases = {'insensitive': ((0, 2), False), 'sensitive': ((0, 2), True),
         'region': ((2,), False)}
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

seen = {name: set() for name in cases}
expected = {name: {'retained_rows': 0, 'bytes': 0, 'hash': hashlib.sha256()}
            for name in cases}
outputs = {name: (root / f'{name}.csv').open('rb', buffering=8 * 1024 * 1024)
           for name in cases}
source_hash = hashlib.sha256()
rows = 0
with (root / 'duplicates.csv').open('rb', buffering=8 * 1024 * 1024) as source:
    lines = RecordingLines(source)
    reader = csv.reader(lines, strict=True)
    assert next(reader) == ['\ufeffname', 'row_id', 'region', 'note']
    header = lines.take()
    source_hash.update(header)
    for name in cases:
        assert outputs[name].read(len(header)) == header, (name, 'header')
        expected[name]['hash'].update(header)
        expected[name]['bytes'] += len(header)
    for row in reader:
        raw = lines.take()
        source_hash.update(raw)
        rows += 1
        for name, (columns, sensitive) in cases.items():
            fields = tuple(row[column].encode('utf-8') if column < len(row) else b''
                           for column in columns)
            key = fields if sensitive else tuple(field.translate(ascii_fold) for field in fields)
            if key in seen[name]:
                continue
            seen[name].add(key)
            result = expected[name]
            result['retained_rows'] += 1
            result['bytes'] += len(raw)
            result['hash'].update(raw)
            assert outputs[name].read(len(raw)) == raw, (name, 'source record', rows)
for name in cases:
    assert outputs[name].read(1) == b'', (name, 'extra output bytes')
    outputs[name].close()
    result = expected[name]
    result['sha256'] = result.pop('hash').hexdigest()
    result['duplicate_rows'] = rows - result['retained_rows']
    assert (root / f'{name}.csv').stat().st_size == result['bytes']
metadata = json.loads((root / 'dataset.json').read_text())
assert source_hash.hexdigest() == metadata['sha256'], 'Source changed'
assert rows == metadata['data_rows']
assert (root / 'duplicates.csv').stat().st_size == metadata['bytes']
assert (metadata['bytes'], rows, source_hash.hexdigest()) == (
    1_000_000_048, 3_666_416,
    '273b272669610a80922e19def23e9c85b6de5fc0a1e2c3ce46f881bba50adcb9')
known = {
    'insensitive': (200_206, 53_724_244, '8c02eb2268452e68c5e0c5c2fd849c531acf0eec2d16bd381fa5638454fb9014'),
    'sensitive': (400_406, 109_141_949, 'ba736bfebe3211ecadbe356746a2465908766238592e8b49d9ba4fd00a9a1931'),
    'region': (8, 1_345, '8214860c61cf4d55cf5911ec8a0dc16fcb97915277f53e2188d43f2717dbaaf2'),
}
for name, (kept, size, sha) in known.items():
    assert (expected[name]['retained_rows'], expected[name]['bytes'], expected[name]['sha256']) == (kept, size, sha)
for name in ['count-only', *cases]:
    log = (root / f'{name}.log').read_text()
    reference = expected['insensitive' if name == 'count-only' else name]
    assert 'Outcome: complete' in log
    for label, count in [('Rows scanned', rows), ('Duplicate rows', reference['duplicate_rows']),
                         ('Retained rows', reference['retained_rows'])]:
        assert int(re.search(rf'^{label}: (\d+)$', log, re.M).group(1)) == count, (name, label)
for name in ['cancel-count', 'cancel-output']:
    log = (root / f'{name}.log').read_text()
    assert 'Outcome: cancelled' in log
    assert 'Destination published: no' in log
    scanned = int(re.search(r'^Bytes scanned: .*?\((\d+) bytes\)', log, re.M).group(1))
    assert 100_000_000 <= scanned < metadata['bytes']
for name in ['count-only', 'cancel-count']:
    log = (root / f'{name}.log').read_text()
    temporary = re.search(r'Analysis candidate cleaned: yes \((.*)\)', log).group(1)
    assert not Path(temporary).exists(), temporary
assert not (root / 'cancelled.csv').exists()
assert not list(root.glob('.quarry-*')), 'Duplicate sort/staging artifact remains'
result = {'source_rows': rows, 'source_sha256_before_and_after': source_hash.hexdigest(),
          'outputs': expected,
          'checks': 'All output records byte-identical to the first original record for each decoded key; original order, BOM/header, CRLF/multiline and final missing terminator preserved; source hash unchanged.'}
(root / 'independent.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result, indent=2))
```
