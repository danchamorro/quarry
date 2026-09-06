# Pre-beta connected workflow closeout: 2026-09-06

The engineering checks in the [pre-beta checklist](../PRE_BETA_CHECKLIST.md#completion-and-release-handoff)
passed in the clean installed application. All four feature priorities are
merged. Final owner signoff remains pending; this report does not claim public
distribution readiness.

## Build and scope

- App: `/Applications/Quarry.app`, verified signature and bundle metadata.
- Revision: `f5efee2eeca33efc7851fef77a7eb51034d3f76f`, source status `clean`.
- Installed signed executable SHA-256:
  `d88496c6b9917b592909a6f2233b895466339eeb554249f1a7b20d44610c2794`.
- Apple M3 Max, Mac15,9, 128 GiB RAM; macOS 26.6.2 (25G83), arm64.
- Locked release build, Rust 1.88.0. PR #38 passed CI, CLA, CodeRabbit,
  and GitGuardian checks before merge. Its review fix passed all 291 workspace
  tests, formatting, strict Clippy, and the locked release build.
- This closeout changes documentation only. It adds an installed-app correctness
  check, not new large-file performance measurements. The earlier
  [1 GB and separate-volume results](2026-09-05-temporary-disk-handling.md)
  retain their original binary provenance.

The clean merged app also passed temporary-folder selection and capacity
inspection, cell editing, text sorting into the selected folder, structural
Undo/Redo, and exact Save As output during merge installation. Its disposable
34-byte result matched SHA-256
`7d87f72ed6793147affb004033cb55022656049a977bdd5913ac66196ad629c2`;
the source remained unchanged and its working directory was cleaned.

## Connected installed-app check

The fixture has seven data rows, a UTF-8 BOM, CRLF record endings, a quoted
comma, an embedded LF in a quoted field, duplicate IDs, numeric boundaries,
and a blank amount. An independent Python verifier uses `csv`, `Decimal`, and
a keep-first set, then compares complete bytes and fixed SHA-256 constants.

1. Open `source.csv`. Change data row 1, column 2 from `Zed` to `Zoe`, then
   rename column 2 from `name` to `display_name`.
2. Undo the header edit, then the cell edit. Confirm the intermediate state
   retains `Zoe` under `name`, and the second Undo returns the file to clean
   with `Zed`. Redo the cell edit, then the header edit independently.
3. Save As `checkpoint.csv`. This establishes the saved document required
   before filtering edited cells. Verify the exact seven-row edited checkpoint
   and the unchanged original source.
4. Filter column 3, `amount`, with **Between (inclusive)** from `500` to `1000`.
   Confirm four matches at original row positions 1, 2, 5, and 6: IDs `A,B,D,B`.
   Both endpoints match; blank, `499.99`, and `1000.01` do not.
5. Export to `filtered.csv`. Verify all four rows, complete bytes, quoting,
   BOM, record endings, and the unchanged source and checkpoint.
6. Clear the filter. Find duplicates using column 1, `id`, with default
   case-insensitive matching. Preview reports two extra rows and five to keep.
   Remove extra rows. Confirm retained IDs `A,B,C,D,E` in original relative order.
7. Undo removal to restore seven rows, then Redo to restore five. Before Save,
   verify the checkpoint is still the seven-row edited file and the original
   remains unchanged. The private five-row working CSV matches the final oracle.
8. Save the checkpoint in place. Verify the exact five-row result, the
   unchanged exported file and original source, and the clean reopened grid.
   Confirm the private working file and its owning directory were removed.

All steps passed using the installed app's controls. Verification recorded the
checkpoint, export, pre-Save, and final stages separately, including proof that
Save had not modified the checkpoint early. The app was returned to an empty,
clean session after validation.

The recorded run checked the private working file separately from the stage
log. The reproduction verifier below includes that comparison in `pre-save`.

| Artifact / stage | Data rows | Bytes | SHA-256 |
| --- | ---: | ---: | --- |
| Original source, unchanged throughout | 7 | 200 | `41e1f7a2628657aa4429639df5f21e272e14e10e79e2c4b090347e3f0cf6ff40` |
| Edited checkpoint before final Save | 7 | 208 | `93165b63ee485baeb20071dcad7ffb35fb2839a4a5beafaa3299f717ad419e0d` |
| Filtered export | 4 | 143 | `a9be9a0cc1a5f2c4b932d816fe280baaaacfa7c7a7c46bc1ec256fb76c0d1366` |
| Checkpoint after duplicate removal and Save | 5 | 141 | `140ac5aa38726e2eb57df5ea19379005f41df515f41826a8b83b28d5a92afe63` |

Local evidence is in `/private/tmp/quarry-pre-beta-closeout-uo2wqrhh`, including
`oracle.json`, `verification.jsonl`, and the captured private working-file path.
These are disposable local artifacts. No application bundle was published.

## Reproduce

Save the following snippets as `generate.py` and `verify.py` in a fresh
directory, using Python 3.11 or later. Run `python3 generate.py`, then follow
the installed-app steps above. Run the verifier at the matching stage:

```sh
python3 verify.py checkpoint
python3 verify.py export
python3 verify.py pre-save "/absolute/path/to/quarry-working-.../generation-1.csv"
python3 verify.py final
```

Run each command at its corresponding point in the GUI sequence, not all at
once after Save. The final verification requires the earlier checkpoint and
export evidence plus a successful pre-Save working-file comparison. Replace
the example path with the active private working CSV's absolute path, captured
before Save removes it. The fixture is small by design and does not exercise the
256 MiB storage-review threshold or substitute for the linked large-file tests.

### generate.py

```python
from pathlib import Path
import csv
import io

root = Path(__file__).resolve().parent
source = b'\xef\xbb\xbfid,name,amount,note\r\nA,Zed,501,"first, note"\r\nB,Ada,1000,"line one\nline two"\r\nA,Zed duplicate,1000.01,outside\r\nC,Mia,,blank\r\nD,Leo,500,boundary\r\nB,Ada duplicate,999.99,duplicate\r\nE,Nia,499.99,low\r\n'
(root / 'source.csv').write_bytes(source)
parsed = list(csv.reader(io.StringIO(source.decode('utf-8-sig'), newline='')))
header, rows = parsed[0], parsed[1:]

def write_expected(name, header, rows):
    stream = io.StringIO(newline='')
    writer = csv.writer(stream, lineterminator='\r\n')
    writer.writerow(header)
    writer.writerows(rows)
    (root / f'expected-{name}.csv').write_bytes(b'\xef\xbb\xbf' + stream.getvalue().encode())

rows[0][1] = 'Zoe'
write_expected('cell-only', header, rows)
header[1] = 'display_name'
write_expected('checkpoint', header, rows)
write_expected('export', header, [rows[i] for i in [0, 1, 4, 5]])
write_expected('final', header, [rows[i] for i in [0, 1, 3, 4, 6]])
```

### verify.py

```python
from pathlib import Path
from decimal import Decimal
from datetime import datetime, timezone
import csv
import hashlib
import io
import json
import sys

ROOT = Path(__file__).resolve().parent
EXPECTED = {
    'source': (200, '41e1f7a2628657aa4429639df5f21e272e14e10e79e2c4b090347e3f0cf6ff40'),
    'cell-only': (200, '509d8d97ed41a67d28c1e0522146229686c28aa100bbb40db8e4d4a3b35e6f7a'),
    'checkpoint': (208, '93165b63ee485baeb20071dcad7ffb35fb2839a4a5beafaa3299f717ad419e0d'),
    'export': (143, 'a9be9a0cc1a5f2c4b932d816fe280baaaacfa7c7a7c46bc1ec256fb76c0d1366'),
    'final': (141, '140ac5aa38726e2eb57df5ea19379005f41df515f41826a8b83b28d5a92afe63'),
}

def signature(data):
    return len(data), hashlib.sha256(data).hexdigest()

def serialize(header, rows):
    stream = io.StringIO(newline='')
    writer = csv.writer(stream, lineterminator='\r\n')
    writer.writerow(header)
    writer.writerows(rows)
    return b'\xef\xbb\xbf' + stream.getvalue().encode('utf-8')

source = (ROOT / 'source.csv').read_bytes()
assert signature(source) == EXPECTED['source'], 'Original source changed'
parsed = list(csv.reader(io.StringIO(source.decode('utf-8-sig'), newline='')))
header, rows = parsed[0], parsed[1:]
assert header == ['id', 'name', 'amount', 'note']
assert len(rows) == 7
assert rows[0] == ['A', 'Zed', '501', 'first, note']
assert rows[1] == ['B', 'Ada', '1000', 'line one\nline two']
assert [row[2] for row in rows] == ['501', '1000', '1000.01', '', '500', '999.99', '499.99']
rows[0][1] = 'Zoe'
cell_only = serialize(header, rows)
header[1] = 'display_name'
checkpoint = serialize(header, rows)
matching_positions = [i for i, row in enumerate(rows) if row[2] and Decimal('500') <= Decimal(row[2]) <= Decimal('1000')]
assert matching_positions == [0, 1, 4, 5]
filtered = [rows[i] for i in matching_positions]
assert [row[0] for row in filtered] == ['A', 'B', 'D', 'B']
export = serialize(header, filtered)
seen = set()
kept = []
kept_positions = []
ascii_fold = str.maketrans('ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz')
for i, row in enumerate(rows):
    key = row[0].translate(ascii_fold)
    if key not in seen:
        seen.add(key)
        kept.append(row)
        kept_positions.append(i)
assert kept_positions == [0, 1, 3, 4, 6]
assert len(rows) - len(kept) == 2
assert [row[0] for row in kept] == ['A', 'B', 'C', 'D', 'E']
final = serialize(header, kept)
computed = {'source': source, 'cell-only': cell_only, 'checkpoint': checkpoint, 'export': export, 'final': final}
for name, data in computed.items():
    assert signature(data) == EXPECTED[name], (name, 'oracle constant mismatch')
    oracle_name = 'source.csv' if name == 'source' else f'expected-{name}.csv'
    assert (ROOT / oracle_name).read_bytes() == data, (name, 'stored oracle mismatch')

stage = sys.argv[1] if len(sys.argv) >= 2 else 'source'
assert stage in ['source', 'checkpoint', 'export', 'pre-save', 'final'], 'Use source, checkpoint, export, pre-save, or final'
assert len(sys.argv) == 3 if stage == 'pre-save' else len(sys.argv) in [1, 2], 'Only pre-save requires an additional working CSV path'
if stage == 'pre-save':
    assert Path(sys.argv[2]).is_absolute(), 'Use the absolute path to the active private working CSV'
checked = {'source.csv': {'bytes': len(source), 'sha256': EXPECTED['source'][1], 'data_rows': 7}}

def check_file(filename, expected, count):
    data = (ROOT / filename).read_bytes()
    assert data == expected, (filename, 'byte mismatch', signature(data), signature(expected))
    assert len(list(csv.reader(io.StringIO(data.decode('utf-8-sig'), newline='')))) - 1 == count
    checked[filename] = {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest(), 'data_rows': count}

if stage == 'source':
    assert not (ROOT / 'checkpoint.csv').exists(), 'Checkpoint unexpectedly already exists'
    assert not (ROOT / 'filtered.csv').exists(), 'Export unexpectedly already exists'
elif stage in ['checkpoint', 'export', 'pre-save']:
    check_file('checkpoint.csv', checkpoint, 7)
    if stage in ['export', 'pre-save']:
        check_file('filtered.csv', export, 4)
    if stage == 'pre-save':
        check_file(sys.argv[2], final, 5)
else:
    check_file('checkpoint.csv', final, 5)
    check_file('filtered.csv', export, 4)
    evidence = [json.loads(line) for line in (ROOT / 'verification.jsonl').read_text().splitlines()]
    assert any(item['stage'] == 'checkpoint' and item['files']['checkpoint.csv']['sha256'] == EXPECTED['checkpoint'][1] for item in evidence), 'Missing earlier edit-only checkpoint verification'
    assert any(item['stage'] in ['export', 'pre-save'] and item['files']['checkpoint.csv']['sha256'] == EXPECTED['checkpoint'][1] for item in evidence), 'Missing earlier export/edit-only checkpoint verification'
    assert any(item['stage'] == 'pre-save' and any(file['sha256'] == EXPECTED['final'][1] for file in item['files'].values()) for item in evidence), 'Missing pre-Save working-file verification'

result = {'stage': stage, 'verified_at_utc': datetime.now(timezone.utc).isoformat(), 'files': checked, 'filter_matches': [1, 2, 5, 6], 'duplicates_removed': 2, 'retained_rows': [1, 2, 4, 5, 7]}
with (ROOT / 'verification.jsonl').open('a') as log:
    log.write(json.dumps(result) + '\n')
print(json.dumps(result, indent=2))
```
