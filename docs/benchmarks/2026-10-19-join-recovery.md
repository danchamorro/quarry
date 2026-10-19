# Join recovery and publication evidence

Environment: VMPal macOS VM 1, macOS 27.0.1 (26A434), Apple Silicon,
16 GiB RAM, Rust 1.88.0. Every input and foreign sentinel is synthetic.
This evidence extends the current-document work; it is not a release sign-off.

## Executed public-job matrix

The new core module runs fourteen tests containing 116 targeted fault or
cancellation scenarios. They use real public analysis/creation jobs, actual
private files and atomic publication. Scoped test-only checkpoints are copied
from the calling thread into its own join worker; other parallel tests are
unaffected. Checkpoints and injected faults are absent from production builds.
Pause guards and finite deadlines prevent a failed assertion from stranding a
worker.

Representative inputs combine two key columns, duplicate expansion, UTF-8 and
Windows-1252, comma/tab delimiters, an empty key component, unmatched rows,
quoted values and a selected/relabelled two-column output. All four join modes
are exercised with 128-byte run and 16-byte group targets to force spill passes.
A complete successful full-join result is compared with a handwritten UTF-8
byte oracle.

| Condition | Observed result |
|---|---|
| Cancel inspection, sorting, a merge, or match analysis | No result; owned private data removed; sources unchanged |
| Cancel pair expansion, ordinal restoration, row writing, flush, or sync | No published result; staging and spills removed |
| Cancel after final source checks, before atomic publication | No published result |
| Cancel after atomic publication | Complete result retained and reported, with exact rows/bytes |
| Output write/flush/sync failure or injected disk-full error | No publication; actionable phase/folder error; sources and existing files retained |
| Private spill write/flush failure during preparation or output ordering | Correct phase and private folder, recovery guidance, no unpublished files |
| Another writer creates the destination immediately before publication | Existing destination bytes retained; no replacement |
| Input changes at the final guard | Join-specific logical input/role diagnostic; changed external bytes retained |
| Foreign file blocks scratch cleanup after cancellation | Cancellation and cleanup failure both visible; foreign data preserved |
| Foreign file blocks scratch or staging cleanup after success | Completed result retained; cleanup warning reported |
| Write/flush/sync failure plus failed staging cleanup | Original I/O failure and cleanup failure both reported |
| Source conflict or cancellation plus failed staging cleanup | Both causes reported; no published result |

A separate disk-full case stops after 64 output write calls in a 32-by-33
many-to-many join. At that checkpoint, at least 8 KiB of CSV bytes have reached
the actual private staging file. Its Unix file/directory permissions are
0600/0700. The injected OS disk-full error then removes the partial staging and
every owned spill while preserving both complete original inputs. Disk-full
errors are injected at I/O boundaries; this does not claim a physically full
volume was mounted.

The desktop harness additionally selects two current unsaved documents, retains
a ready preview, changes one saved input externally, and attempts creation
through the real accessible control. Failure releases both source leases and
the operation slot, retains both edit histories and unsaved values, preserves
the external replacement, and does not create an output.

## Regressions and fixes

The cancellation check failed before the fix because a cancellation arriving
during final source/identity I/O could still publish a file. The shared writer
now checks cancellation immediately before atomic publication as well as before
the guards. The test passes for every join mode; post-publication cancellation
still reports completion.

Private spill failures initially returned an unlocated I/O error. They now
identify the active join phase and actual private folder and give a recovery
action. Join output errors also identify their stage. Shared multi-source
validation errors are adapted to the join's logical input and role.

The write-plus-cleanup test initially lost the original write error. Extending
it to flush/sync exposed cleanup failures hidden by best-effort destruction.
Publication now gathers the primary outcome and explicit staging cleanup
separately. Join reports both failures; a successful publication remains
successful with a cleanup warning. Existing export/save call signatures and
normal outcomes remain unchanged.

Failure-before logs in the VM:

- /tmp/quarry-127-publication-red.log
- /tmp/quarry-127-spill-diagnostic-red.log
- /tmp/quarry-127-error-context-red.log
- /tmp/quarry-127-publication-secondary-cleanup-red.log

Passing matrix evidence is in /tmp/quarry-127-combined-failures-green.log.
The combined workspace passed 671 tests in /tmp/quarry-127-workspace.log.
All-target/all-feature Clippy with warnings denied passes in
/tmp/quarry-127-clippy-final.log.
The native Windows CI job now includes this portable fault module; its updated
results must be inspected rather than inferred from macOS.

## Remaining release evidence

The full #71 acceptance audit, final native UI matrix and release-mode scale
measurements remain #128 gates. This matrix does not establish throughput,
peak RSS/disk or cancellation latency for inputs larger than RAM. It also does
not replace native verification of the merged installed VM application, required
CI, or review of the actual final commit.
