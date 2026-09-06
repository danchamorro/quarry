# ADR 0005: Selected working folders and advisory capacity checks

- Status: Accepted
- Date: 2026-09-05

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
