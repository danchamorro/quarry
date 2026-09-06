# Temporary-disk handling validation: 2026-09-05

Pre-beta priority 4 preserves the existing sort and duplicate output while
checking writable capacity and allowing a selected working directory. The
1 GB before/after validation below passed against the same deterministic source.
The [priority checklist](../PRE_BETA_CHECKLIST.md#4-temporary-disk-handling)
tracks integrated checks and installed-app validation.

## Environment and scope

- Apple M3 Max, Mac15,9, 137,438,953,472 bytes RAM (128 GiB).
- macOS 26.6.2 (25G83), rustc 1.88.0 (6b00bc388 2025-06-23).
- Release profile and locked dependencies.
- Baseline source: clean archive of merged commit
  `9210369d5d0953d0f365f5d61c5586f788a8e5df`.
- Baseline binary SHA-256:
  `b76ea732b6f118c8aad3a9596bdb622d1e8b1907b58d6fe519388066a52e10ec`.
- Feature source: local uncommitted `codex/temporary-disk-handling` changes
  based on the same commit, before the final private-output handoff fix.
- Feature binary SHA-256:
  `39460fe6b13c76b516698b0bc3ff7820e4dc8fc646138dd63b484325164042fb`.
- Each phase fully read and hashed the source before running sequential jobs,
  so the filesystem cache was warm. Other agent builds and tests were paused
  during measurements; unrelated system load was not controlled.
- Source, published output, and selected alternate folder were on the same
  APFS Data volume. These timings do not measure cross-device throughput.
  Separate functional checks below used a mounted 64 MiB HFS+ test volume.

These are single local observations, not a speedup claim. The engine now makes
an additional bounded, cancellable record-count pass before materializing
sort/duplicate data so it can check an estimate without treating every byte as
a row. Job time includes that preflight. Sort command wall time also includes
its existing indexing, source/output fingerprints, and output validation.

## Deterministic input and exact output

The source is the same generator and fixture documented in
[duplicate-removal validation](2026-09-05-find-remove-duplicates.md#deterministic-workload):
1,000,000,048 bytes, 3,666,416 data records, and columns
`name,row_id,region,note`. It exercises BOM, CRLF, multiline quoted values,
escaped quotes, alternative quoting, empty and missing fields, case variants,
and a final unterminated record.

The source SHA-256 matched before and after every measurement phase:
`273b272669610a80922e19def23e9c85b6de5fc0a1e2c3ce46f881bba50adcb9`.

| Output | Data rows | Bytes | SHA-256, identical before and after |
| --- | ---: | ---: | --- |
| Stable ascending text sort, column 1, case-sensitive CLI | 3,666,416 | 1,000,000,050 | `d9d7ce1286f8e9b75225d3763ecd5aff184a1c170ecb62d573da312be44c5c31` |
| Duplicates on columns 1,3, default ASCII-insensitive matching | 200,206 | 53,724,244 | `8c02eb2268452e68c5e0c5c2fd849c531acf0eec2d16bd381fa5638454fb9014` |

Duplicate analysis reported 3,466,210 extra occurrences. Its output also matches
the previously published independent raw-record oracle. Sort's complete CLI
validation checked ordering, row/header counts, source preservation, bounded
record-multiset evidence, and stable ties. Reordering the unterminated record
requires a line terminator between records, explaining the two extra output bytes.

## Measurements

Peak RSS comes from `/usr/bin/time -l`. Peak temporary bytes are engine-tracked
payload for run files and staged output, excluding source bytes and filesystem
metadata. Existing retained files remain allocated and are already reflected
in filesystem available space; they are not counted again as new allocation.

| Build and operation | Job time | Command wall time | Peak RSS | Peak temporary bytes |
| --- | ---: | ---: | ---: | ---: |
| Baseline sort | 6.025 s | 11.497 s | 29.81 MiB | 2,362,291,799 |
| Selected-folder sort | 5.776 s | 10.841 s | 32.80 MiB | 2,362,291,799 |
| Baseline duplicates with output | 5.110 s | 5.123 s | 51.09 MiB | 1,967,567,111 |
| Selected-folder duplicates with output | 4.972 s | 4.998 s | 52.16 MiB | 1,967,567,111 |
| Selected-folder count-only duplicates | 5.074 s | 5.088 s | 50.33 MiB | 1,967,567,111 |

The final CLI sort displayed an 8,307,979,356-byte conservative allowance and
902,349,099,008 available bytes before starting. It uses the shared estimator,
including potential reserialization of BOM-looking values and bare CR fields.
The worker checks the relevant volumes again before writing. Available space
is advisory and can change after the check.

The selected folder was
`/private/tmp/quarry-temporary-disk-validation-2026-09-05/alternate`.
A 20 ms polling observer saw private run files there during every feature run.
The folder was empty after every completed or cancelled operation. Count-only
analysis also removed its private candidate. Explicit output remained at its
requested destination, with no unpublished `.quarry-*` files left beside it.

## Cancellation

Both jobs requested cancellation after scanning at least 100,000,000 source
bytes, after the capacity preflight and while private runs already existed.
The completed preflight is included in job time, not in cancellation latency.

| Job | Observed request point | Cancellation latency | Peak RSS | Peak temporary bytes |
| --- | ---: | ---: | ---: | ---: |
| Selected-folder sort | 100,663,296 bytes | 1.451 ms | 27.70 MiB | 134,216,190 |
| Selected-folder duplicates | 101,711,872 bytes | 1.980 ms | 31.28 MiB | 134,216,366 |

Both returned `cancelled`, published no output, removed their observed private
runs, and left the source SHA-256 unchanged. This is cancellation during run
creation, not a claim about every cancellation point or abrupt power loss.

## Cross-volume output and real capacity rejection

A task-owned 64 MiB HFS+ image mounted at
`/Volumes/QuarryStorageValidation` provided a genuinely different filesystem
for functional checks. The observed device IDs were `16777232` for local APFS
and `16777237` for HFS+. The CLI used isolated scratch/output directories so
native app validation could share the mounted volume.

The 59-byte fixture contained a BOM, CRLF, three data rows, and a quoted
multiline value. All four commands passed, with exact bytes checked:

| Scratch volume | Published output volume | Operation | Output SHA-256 |
| --- | --- | --- | --- |
| HFS+ | APFS | Descending column-1 sort | `7495a9e66b4e56466db4db0eb76e0c0b0aba67cbf608d7af138a10da517cca69` |
| APFS | HFS+ | Descending column-1 sort | `7495a9e66b4e56466db4db0eb76e0c0b0aba67cbf608d7af138a10da517cca69` |
| HFS+ | APFS | Column-1 duplicate analysis/output | `0592beaa84729c308a68b380bd7ec7e0dd7974d4e89c23e4abb88144e104a162` |
| APFS | HFS+ | Column-1 duplicate analysis/output | `0592beaa84729c308a68b380bd7ec7e0dd7974d4e89c23e4abb88144e104a162` |

Every output contained 59 bytes. All three keys were distinct, so duplicate
output exactly matched the source. The source hash remained
`0592beaa84729c308a68b380bd7ec7e0dd7974d4e89c23e4abb88144e104a162`.

The 1 GB benchmark source then exercised actual insufficient-capacity errors
without filling the test volume:

| Limited location | Reported additional requirement | Available bytes | Result |
| --- | ---: | ---: | --- |
| HFS+ scratch, APFS output | 8,307,979,356 | 65,490,944 | Rejected before materialization |
| APFS scratch, HFS+ output | 2,014,665,767 | 65,490,944 | Rejected before materialization |

Each command returned exit code 1 with the affected location, required and
available bytes, and guidance to free space or choose another location.
Neither published an output. CLI-owned scratch directories were empty after
success and failure, and neither source changed. This checks both filesystem
identities and independent output-volume capacity rather than inferring them
from two directories on one volume.

The same measured feature binary was used. Commands, outputs, exact hashes,
and errors are preserved in `cross-volume.json`, `cross-*.log`, and
`cross-volume.py` in the local validation directory. The desktop owner manages
the test mount lifecycle; the CLI checks did not detach it.

## Automated and installed-app checks

All 290 workspace tests passed on the final code: 142 core, 109 egui, 29 CLI, nine delimited,
and one AppKit. Formatting, strict all-targets/all-features Clippy, and the
locked release workspace build passed.

These final automated checks include the later change that keeps private
output in staging until the caller consumes `wait()`. Core regressions cover
abandonment, destination conflicts, source changes while awaiting handoff,
cancellation, publication, and staging permissions. Existing GUI and CLI
regressions passed against the same code. The recorded 1 GB measurements,
cross-volume checks, and installed-app checks below predate that change and
were not repeated afterward; their binary hashes identify the earlier build.

The CLI checks include a new combined regression for selected-folder
sort, duplicate output, count-only cleanup, unavailable locations, and a file
passed as a directory. The existing duplicate cancellation regression now
uses a selected folder and checks it is empty afterward. CLI help lists
`--temp-dir DIRECTORY` for sort and duplicates and explains that output staging
stays beside the destination.

Core tests cover insufficient capacity, write failures, bound calculations,
and private-file cleanup. The large-file benchmark did not fill or disconnect
a volume; the separate HFS+ tests exercised actual capacity rejection.
The installed feature build was packaged, installed, and verified locally from
`9210369` plus the uncommitted changes. Its metadata records **dirty source**;
the installer preserved its verified rollback archive. The installed, signed
executable SHA-256 is
`7279410073615da82c260b463d375833a273b2d94bc3e01bfaf9dcb01061053b`.
No app bundle was published.

Native accessibility and visual checks passed for **File → Temporary storage**:

- A nonexistent folder showed its path and an actionable error, and **Use
  folder** remained disabled. Changing the path invalidated the old capacity
  result until **Check space** ran again.
- Selecting the isolated HFS+ volume displayed 62.46 MiB available. A column
  deletion created the exact 29-byte BOM/CRLF working file on that volume.
- Choosing an APFS working folder while that version was retained displayed
  its 29-byte allocation separately. Deleting a row created the exact 24-byte
  next generation on APFS. The HFS+ version remained intact; Undo reopened it
  and Redo restored the APFS result.
- The 59-byte fixture source stayed byte-identical until explicit Save. Save
  produced `b'\xef\xbb\xbfkey,value\r\na,1\r\nc,3\r\n'`, SHA-256
  `ccadef86b524511c321b91db98502b69dc2389a3e6bc7e63fca48d56715cc27a`,
  and removed both private working directories. This intentional GUI Save
  followed the cross-volume CLI checks above.
- Sorting the 1 GB fixture paused at **Review storage requirements** before
  writing. The 7.74 GiB allowance exceeded the test volume's 62.46 MiB;
  **Continue** was disabled. Cancel restored the unchanged document and created
  no working directory.

- Continuing the same large sort on APFS completed normally. With **Match
  case** enabled to match the CLI benchmark's case-sensitive comparison, the
  installed app produced exactly 1,000,000,050 bytes and the published sort
  SHA-256 above. The 1 GB source hash stayed unchanged. A retained 953.67 MiB
  Redo file was shown separately in the review, then removed only after the
  next successful operation superseded it. Undo restored the original source.

Clean shutdown removed all remaining working/Redo directories. The test volume
was detached, and the installed app was reopened with no file open and its
default session settings. All 146 local documentation links and anchors checked
across the updated guides, checklist, roadmap, ADR, and report resolved.

The temporary test volume image and GUI fixtures are local validation artifacts under
`/private/tmp/quarry-storage-ui-7mo5yo3p`. The source fixture starts with:

```python
b'\xef\xbb\xbfkey,value,note\r\na,1,"first\nline"\r\nb,2,second\r\nc,3,last\r\n'
```

Late write failure is exercised by automated faults after a successful capacity
check, including output writes and buffered flush. It does not depend on filling
the user's drive. Tests assert source and retained Undo preservation, removal of
unpublished staging, and cleanup of completed private candidates abandoned
without consuming their result. Public Save As outputs are retained.

## Reproduction

Use Python 3.11 or later. Save `generate.py` from the linked duplicate benchmark
and the `run.py` below into a fresh validation directory. The generator writes
the same 1 GB fixture there. From the repository root, run:

```sh
storage_validation_dir="$(mktemp -d /private/tmp/quarry-storage-validation-XXXXXX)"
# Save generate.py and run.py in "$storage_validation_dir" first.
python3 "$storage_validation_dir/generate.py"
mkdir "$storage_validation_dir/base" "$storage_validation_dir/alternate"
git archive 9210369d5d0953d0f365f5d61c5586f788a8e5df | tar -x -C "$storage_validation_dir/base"
cargo build --manifest-path "$storage_validation_dir/base/Cargo.toml" -p quarry-cli --release --locked --target-dir "$storage_validation_dir/base-target"
cp "$storage_validation_dir/base-target/release/quarry" "$storage_validation_dir/quarry-base-9210369"
cargo build -p quarry-cli --release --locked
cp target/release/quarry "$storage_validation_dir/quarry-after"
python3 "$storage_validation_dir/run.py" before
python3 "$storage_validation_dir/run.py" after
```

The runner fails on a command/metrics error, wrong source or output hash,
incorrect counts, unobserved selected-folder use, or leftover temporary output.
It records exact invocations, exit codes, metrics, and observed scratch paths
in `before-commands.json` and `after-commands.json`. Machine timings and available
space will vary. Binary hashes can vary with the build environment and path.
The reported local artifacts are in
`/private/tmp/quarry-temporary-disk-validation-2026-09-05`.

### run.py

```python
from pathlib import Path
import hashlib,json,re,subprocess,sys,time
root=Path(__file__).resolve().parent
source=root/'duplicates.csv'
expected_source='273b272669610a80922e19def23e9c85b6de5fc0a1e2c3ce46f881bba50adcb9'
expected_duplicate='8c02eb2268452e68c5e0c5c2fd849c531acf0eec2d16bd381fa5638454fb9014'
def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream,'sha256').hexdigest()
assert source.stat().st_size==1_000_000_048
assert digest(source)==expected_source
phase=sys.argv[1]
assert phase in ['before','after']
binary=root/('quarry-base-9210369' if phase=='before' else 'quarry-after')
scratch=root/'alternate'
assert not list(scratch.iterdir())
options=[] if phase=='before' else ['--temp-dir',str(scratch)]
sort_options=['--column','1','--order','asc','--mode','text','--header','first-row','--cache-state','warm']
duplicate_options=['--columns','1,3','--header','first-row','--cache-state','warm']
jobs=[('sort',['sort-save-as',str(source),str(root/f'{phase}-sort.csv'),*sort_options,*options],root/f'{phase}-sort.csv'),
      ('duplicates',['duplicates',str(source),'--output',str(root/f'{phase}-duplicates.csv'),*duplicate_options,*options],root/f'{phase}-duplicates.csv')]
if phase=='after':
    jobs.extend([
        ('count-only',['duplicates',str(source),*duplicate_options,*options],None),
        ('cancel-sort',['sort-save-as',str(source),str(root/'after-cancel-sort.csv'),*sort_options,*options,'--cancel-after-bytes','100000000'],root/'after-cancel-sort.csv'),
        ('cancel-duplicates',['duplicates',str(source),'--output',str(root/'after-cancel-duplicates.csv'),*duplicate_options,*options,'--cancel-after-bytes','100000000'],root/'after-cancel-duplicates.csv'),
    ])
results=[]
for name,args,output in jobs:
    if output: assert not output.exists(),output
    log=root/f'{phase}-{name}.log'
    command=['/usr/bin/time','-l',str(binary),*args]
    observed=set()
    started=time.perf_counter()
    with log.open('w') as stream:
        process=subprocess.Popen(command,stdout=stream,stderr=subprocess.STDOUT)
        while process.poll() is None:
            for item in scratch.rglob('*'):
                observed.add(str(item.relative_to(scratch)))
            time.sleep(.02)
        code=process.wait()
    result={'name':name,'command':command,'exit_code':code,'command_wall_seconds':time.perf_counter()-started,'observed_scratch_paths':sorted(observed)}
    results.append(result)
    (root/f'{phase}-commands.json').write_text(json.dumps(results,indent=2)+'\n')
    assert code==0,(name,code,log.read_text())
    text=log.read_text()
    result['peak_rss_bytes']=int(re.search(r'(\d+)\s+maximum resident set size',text).group(1))
    result['peak_temporary_bytes']=int(re.search(r'Peak temporary disk: .*\((\d+) bytes\)',text).group(1))
    result['job_seconds']=float(re.search(r'(?:Sort|Duplicate) wall time: ([\d.]+) s',text).group(1))
    cancelled=name.startswith('cancel-')
    assert 'Outcome: '+('cancelled' if cancelled else 'complete') in text
    if cancelled:
        assert not output.exists()
        result['cancellation_latency_ms']=float(re.search(r'Cancellation latency: ([\d.]+) ms',text).group(1))
        result['cancellation_requested_at']=int(re.search(r'Cancellation requested after: .*\((\d+) bytes\)',text).group(1))
    elif output:
        result['output_bytes']=output.stat().st_size
        result['output_sha256']=digest(output)
        if name=='duplicates':
            assert result['output_bytes']==53_724_244
            assert result['output_sha256']==expected_duplicate
            assert 'Duplicate rows: 3466210' in text and 'Retained rows: 200206' in text
        elif name=='sort':
            assert (result['output_bytes'],result['output_sha256'])==(1_000_000_050,'d9d7ce1286f8e9b75225d3763ecd5aff184a1c170ecb62d573da312be44c5c31')
            assert 'Rows sorted: 3666416' in text
            if phase=='after': assert digest(root/'before-sort.csv')==result['output_sha256']
    elif name=='count-only':
        assert 'Duplicate rows: 3466210' in text and 'Retained rows: 200206' in text
        assert 'Analysis candidate cleaned: yes' in text
    assert not list(scratch.iterdir()),list(scratch.iterdir())
    assert not list(root.glob('.quarry-*'))
    result['scratch_empty_after']=True
    if phase=='after': assert observed,(name,'did not observe scratch artifacts')
    (root/f'{phase}-commands.json').write_text(json.dumps(results,indent=2)+'\n')
    print(json.dumps({k:v for k,v in result.items() if k not in ['command','observed_scratch_paths']}),flush=True)
assert digest(source)==expected_source
print('Source SHA-256 unchanged; all scratch and cancelled output cleaned.',flush=True)
```
