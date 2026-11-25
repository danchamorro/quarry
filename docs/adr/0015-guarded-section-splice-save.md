# ADR 0015: Guarded Section Splice-Save and Section Editing Contract

**Status:** Accepted (`2026-11-24`, Owner sign-off `T13`)
**Date:** 2026-11-24
**Amends:** [ADR 0005: Selected working folders and advisory capacity checks](0005-temporary-storage.md), [ADR 0007: Independent document tabs](0007-document-tabs.md)
**Related:** [ADR 0014: Large File Controller, Bounded Section Windows, and Boundary Trust Rules](0014-large-file-controller-and-section-windows.md)

## Context

[ADR 0014](0014-large-file-controller-and-section-windows.md) introduced bounded section documents (`Session::open_section`) with checked `u64` byte windows, outward record-boundary snapping, `BoundaryTrust::{Verified, Speculative}`, pausable `VerifyBoundaryJob`, and **Save Section As New File**.

Users editing a narrow slice of a multi-gigabyte or multi-terabyte delimited file also need to apply cell edits, row insertions/deletions, and Replace All within that section and write those changes safely back into the parent file without rewriting untouched prefix bytes on Copy-on-Write filesystems, without tearing multiline quoted records, and without violating Quarry's guarded atomic publication and multi-tab integrity rules.

## Decision

### 1. Strict `BoundaryTrust::Verified` Gate

- In-place **Guarded Section Splice-Save** strictly requires `BoundaryTrust::Verified` (established either by `BoundaryProof::CountedScan` or `BoundaryProof::QuoteFreeConfirmed`).
- `BoundaryTrust::Speculative` sections **cannot** splice-save into the parent file (`QuarryError::InvalidOption`) and remain restricted to **Save Section As New File** until **Verify Boundary** completes.

### 2. Allowed vs. Disabled Operations on Sections

- **Cell edits, Undo/Redo, Find, view-only Filter, and Save Section As New:** Available on section documents. Splicing a filtered section reads the section's full backing rows (`[aligned_start, aligned_end)` or its private working copy) so every hidden row is preserved byte-for-byte.
- **Section-scoped row mutations (`BoundaryTrust::Verified` only):** **Insert Rows…**, **Delete Selected Rows**, and **Replace All** operate strictly within the section's byte span, producing private section-scoped working copies (containing only the section's own records without prepending the byte-0 header on interior sections) that participate in Undo/Redo and splice cleanly back into `[aligned_start, aligned_end)`.
- **Disabled for in-place section splice:** Column insert, delete, split, combine, move/reorder, column cleanup, and header renames are disabled on section documents (schema-changing or whole-table operations such as Sort, Dedupe, and Join route to **Save Section As New File** instead).
- **Byte-preserving serialization:** Untouched records within the section copy raw bytes verbatim; an unedited section splices back byte-identical to the original parent file. When splicing an interior section (`aligned_end < parent_file_size`), serialized section records preserve their record terminator so the last section record never merges with the first suffix record. Deleting every row in a section splices zero bytes cleanly, joining `[0..aligned_start)` directly to `[aligned_end..eof)`.

### 3. Consistent-Snapshot Copy-on-Write & Streaming Mechanics

All staging artifacts (`section.stage`, `snapshot.stage`, and `output.stage`) are created inside a private `0700` staging directory on the parent file's volume (`private_staging_directory(parent_dir)`), guaranteeing atomic same-volume `rename` and RAII cleanup on error, `ENOSPC`, or cancellation with zero orphaned temporary files.

1. **Section serialization:** Stream the edited section (from `[aligned_start, aligned_end)` of the parent file or `[0, working_len)` of a section working copy, applying sparse cell edits) into `section.stage` to obtain the exact `new_section_len` with `O(1)` memory.
2. **macOS APFS (`fclonefileat`):**
   - Whole-file CoW clone the open parent file descriptor to `output.stage` inside the private staging directory (immediately restricting staging mode to `0600`).
   - **Same-length splice (`new_section_len == old_section_len`):** `pwrite` / positional-write `section.stage` directly into `output.stage` at `aligned_start`. Neither prefix nor suffix is copied.
   - **Length-changing splice (`new_section_len != old_section_len`):** Create a **second intact CoW snapshot clone** (`snapshot.stage`, zero data copy) of the parent file before calling `ftruncate` (`set_len(aligned_start)`) on `output.stage`. Append `section.stage` at `aligned_start`, unlink `section.stage`, stream `[aligned_end..snapshot_len)` from `snapshot.stage` into `output.stage` (checking cancellation every chunk), and unlink `snapshot.stage`.
3. **Linux (`FICLONE`):**
   - Whole-file CoW clone via `rustix::fs::ioctl_ficlone` to `output.stage` (and `snapshot.stage` when length changes).
   - Same-length edits overwrite `[aligned_start, aligned_end)` in place in `output.stage`.
   - Length-changing edits `ftruncate(aligned_start)`, append `section.stage`, unlink `section.stage`, and stream `[aligned_end..snapshot_len)` from `snapshot.stage` with bounded chunk reads and cancellation checks (block-aligned `FICLONERANGE` suffix cloning may be added later as a transparent optimization).
4. **Non-CoW fallback (ext4, tmpfs, network volumes, Windows, or unsupported clone ioctls):**
   - Stream `[0..aligned_start)` from the parent file, then `section.stage`, then `[aligned_end..parent_file_size)` into `output.stage`.
5. **Advisory disk preflight (ADR 0005 amendment):**
   - Report exact volume free space and check advisory capacity before writing: `section + suffix` on CoW-capable volumes, and full output size (`prefix + section + suffix`) on non-CoW volumes.
6. **Guarded atomic publication (ADR 0005):**
   - Reject symlinks on the parent path. Restore parent permissions and macOS ACLs on `output.stage`, call `sync_all()`, check cancellation, revalidate `SourceStamp` against the open parent descriptor and parent path (plus working copy stamp when applicable), and atomically `rename` `output.stage` over the parent path.
   - As in ADR 0005, a narrow OS check-to-rename window remains inherent to POSIX atomic `rename` without mandatory file locks; any modification detected before rename aborts cleanly with `QuarryError::SourceChanged` and leaves the parent file untouched.

### 4. Multi-Tab & Parent/Section Coordination (ADR 0007 Amendment)

- **Parent reservation:** While a section splice-save is running, the parent file path (including canonical and hard-link aliases) is reserved in `write_destination`, blocking conflicting saves, exports, or structural writes to the same file from any window.
- **Dirty mutual exclusion:** Across all windows and hard-link aliases (including hydrated and dormant dirty tabs), **at most one** tab for a given underlying file (either the whole-file parent tab OR a single section tab) may hold unsaved edits at a time. Attempting to edit a second sibling/parent tab while another is dirty is rejected with an actionable message.
- **Post-splice tab refresh:** After a successful splice-save, the spliced section tab reloads cleanly at `[aligned_start, aligned_start + new_section_len)` preserving `BoundaryTrust::Verified` (and exact `first_global_row` when counted), while any open clean parent or sibling section tab for that file is immediately marked `source_changed = true` with `SOURCE_CHANGED_NOTICE` rather than silently reloading.

## Consequences

- Editing and saving a 100 MiB section of a multi-gigabyte APFS or btrfs/XFS file avoids copying the untouched prefix (`[0..aligned_start)`), and same-length edits avoid copying the suffix as well.
- Keeping a second intact CoW snapshot clone before `ftruncate` guarantees that suffix streaming reads a consistent snapshot even if an external process mutates the parent file mid-splice (which `SourceStamp` revalidation then detects and rejects before publication).
- Section working copies remain private, bounded to the section's byte span, and fully compatible with Undo/Redo, dirty dormancy, and FD budgets without introducing any new third-party dependencies.
