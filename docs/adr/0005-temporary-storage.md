# ADR 0005: Selected working folders and advisory capacity checks

- Status: Accepted (amended 2026-11-22)
- Date: 2026-09-05
- Amended: 2026-11-22 (persisted scratch directory, adaptive 300 MB single-run memory budget, and `mmap` deferral)

## Context

Whole-file edits, sort runs, duplicate candidates, and adjacent Undo versions
can exceed the available space on the system temporary volume. A source file
must survive both an insufficient-space check and a later write failure.
Moving atomic Save staging to an arbitrary temporary drive would weaken the
existing publication contract.

## Decision

Keep directory choice at the application boundary. The GUI selects a working
folder for the current app session. Existing working and Undo/Redo files remain
owned until obsolete, even after a folder change. The CLI offers `--temp-dir`
for sorting and duplicates. Existing core entry points retain their defaults;
additional entry points accept a scratch directory.

Use one shared core capacity inspector and conservative output estimates.
Inspect the real volume and probe private-file creation, writing, and removal.
Account for output expansion and simultaneous spill generations. Count logical
records with a bounded cancellable pass where the engine has no complete index;
do not substitute source bytes for rows. Already-retained versions reduce free
space naturally and are not charged twice as additional bytes.

The GUI reviews allowances of at least 256 MiB before writing, including the
chosen path, additional allowance, available space, and retained versions.
The threshold avoids adding a confirmation to every small edit. Engine checks
apply at every size and repeat before data writes. Storage failures carry the
target path and actionable context instead of masquerading as source changes.

Preserve destination-adjacent staging for Save and export. When scratch and
output volumes differ, check both. Capacity checks cannot reserve free space;
existing guarded publication and RAII cleanup remain responsible for handling
space consumed after preflight.

Private working-copy, sort, and duplicate outputs stay in job-owned staging
until the caller consumes `wait()`. The worker flushes and syncs before reporting
readiness, then waits for acceptance or cancellation. Acceptance rechecks the
source and publishes without replacing an existing destination, still on the
worker thread. Drop and cancellation discard only staging. They never unlink a
caller-selected destination that another file could have replaced. Public Save,
Save As, and export keep their existing publication timing. For private jobs,
`progress.done` now means ready to consume, not successful publication.

## Alternatives considered

- Always use the system temporary directory: cannot recover by selecting a
  larger drive.
- Move every temporary file to the selected folder: breaks the same-volume
  atomic publication contract.
- Reserve the estimated capacity: over-reserves conservative bounds and adds
  filesystem-specific allocation behavior without eliminating write failures.
- Estimate rows from file size: can reject practical files by overstating sort
  or transformation requirements by orders of magnitude.
- Check file identity before deleting a completed private destination: still
  leaves a race between checking and unlinking a caller-selected path.
- Return ownership-bearing temporary-result objects: safe, but requires a wider
  core API and caller migration than delaying private publication until `wait()`.

## Consequences and validation

New work can move to another drive without copying or discarding the active
Undo history. Users must keep drives holding retained versions connected.
Preflight counting adds a source scan to affected operations while keeping RAM
bounded. The [validation report](../benchmarks/2026-09-05-temporary-disk-handling.md)
records before/after measurements, exact outputs, capacity and write failures,
alternate folders, cancellation cleanup, and installed-app verification for the
original temporary-storage implementation. The later private-handoff change is
covered separately by core regressions for abandonment, destination conflicts,
source changes while awaiting handoff, cancellation, publication, and staging
permissions. The earlier installed-app checks do not validate that later change.

## Amendment (2026-11-22): Persisted scratch folder, adaptive 300 MB single-run budget, and `mmap` deferral

### Persisted scratch folder with strict revalidation

The desktop application now persists a user-selected custom temporary working folder
across restarts in an owner-only preferences file (`storage-preferences`, `0600` on
Unix). Selecting `"Use system temporary folder"` clears the persisted custom path back
to the OS default.

Every persisted custom scratch folder is revalidated on startup (non-blocking) and
strictly before every operation that writes temporary or working-copy files:

- Must be a non-empty absolute path to an existing real directory.
- Must not traverse or target a symbolic link or Windows reparse point (allowing macOS's
  OS-level `/var` and `/tmp` root links).
- Must be owned by the current user (`uid == geteuid()` on Unix; owner SID match on
  Windows).
- Must support creating, writing, and removing a private temporary workspace with
  owner-only permissions (`0700` and cleared inherited ACLs on macOS).

If revalidation fails (for example, an external drive was disconnected, unmounted,
replaced by a symlink, or no longer enforces private permissions), Quarry surfaces a
warning notice and requires the user to choose another folder or explicitly return to
the system temporary folder. Quarry **never** silently falls back to another volume.

### Adaptive single-run memory budget (300 MB default)

External sort uses a configurable tracked-payload single-run memory budget defaulting to
**300,000,000 bytes** (300 MB decimal), capped at 25% of host physical RAM (floored at
16 MiB) and drawn from the engine pool's shared out-of-core byte budget across the
two-job cap.

Rather than maintaining a second in-RAM code path, run generation fills one contiguous
tracked-payload arena per active worker, reserving budget capacity before admitting
rows. If the input finishes within budget, Quarry sorts the in-memory arena(s) and
streams directly to the guarded `ExportTarget` through the exact same comparator,
`OutputVerifier`, and `StructuralIndex` builder with zero temporary spill files. If the
input exceeds the budget or another concurrent job claims shared budget, workers spill
seamlessly to a lazily created private `SpillWorkspace` and continue externally through
bounded parallel intermediate merges and a serial verified final merge.

### `mmap` deferral on Quarry-owned runs

Memory-mapping (`mmap`) remains explicitly deferred even for Quarry-owned spill runs:
an unplugged external scratch volume can raise unrecoverable `SIGBUS`, sequential
positional reads (`pread`/`seek_read` with OS read-ahead hints) already saturate local
storage without mapping risks, and file-backed page mappings distort RSS accounting.
