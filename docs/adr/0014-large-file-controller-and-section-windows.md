# ADR 0014: Large File Controller, Bounded Section Windows, and Boundary Trust Rules

**Status:** Accepted (`2026-11-24`, Owner sign-off `T11`)
**Date:** 2026-11-24
**Related:** [ADR 0001: Initial engine architecture](0001-initial-engine.md), [ADR 0005: Editing and safe saving](0005-editing-and-safe-saving.md), [ADR 0007: Document tabs and dormancy](0007-document-tabs.md)

## Context

Quarry's structural index and out-of-core engine handle multi-gigabyte files cleanly, but users inspecting or slicing multi-gigabyte to multi-terabyte files (including sparse or archival datasets up to 16–20 TiB) need to open a bounded byte window directly without waiting to scan the entire file from byte 0.

Opening an interior byte slice of a delimited file introduces two subtle correctness hazards:

1. **JavaScript/IEEE-754 precision loss:** `f64` loses single-byte integer precision above `2^53` bytes (`8 PiB`, and even coarse rounding across sliders can drift by megabytes). Range calculations must stay strictly in checked `u64` integer arithmetic.
2. **Unproven record boundaries in quoted CSV:** An interior newline byte (`\n`) in a quoted CSV file might lie *inside* a multiline quoted field rather than terminating a record. Treating an arbitrary interior newline or even UTF-16 code-unit offset as a proven record boundary would silently corrupt row framing or enable unsafe splice-back writes.

## Decision

Introduce a bounded section document contract (`Session::open_section`) governed by checked `u64` range math, outward record-boundary snapping, and an explicit two-tier boundary trust model:

### 1. Checked `u64` Range Math & Outward Snapping

- Represent byte windows as `ByteWindow { base: u64, len: u64 }` with checked overflow arithmetic (`base.checked_add(len)`).
- UI controls expose exact unit-aware integer inputs (**From**, **To**, **Size** under one consistent explicitly labelled unit: Bytes, KiB, MiB, GiB, or TiB) as the primary accessible control, paired with coarse 1% sliders for exploration.
- Outward snapping expands the requested `[base, base + len)` interval to include every logical record it intersects while **preserving** any endpoint that already lies exactly on a record boundary.
- Both requested and snapped byte intervals are retained and displayed to the user.

### 2. Header Resolution (Byte 0 Once)

- Dialect and header configuration are resolved once from byte 0 of the parent file via bounded bootstrap sampling.
- Interior sections (`snapped.base > 0`) carry the resolved header row as column metadata without injecting it into the section's byte range or data rows.
- **Save Section As New File** writes the resolved byte-0 header (with any active header renames) before the section's data rows when `dialect.has_header` is true and `snapped.base > 0`.

### 3. Two-Tier Boundary Trust Contract (`BoundaryTrust`)

| Trust Level | Proof (`BoundaryProof`) | How Established | Global Row Ordinals |
|---|---|---|---|
| `BoundaryTrust::Verified` | `CountedScan { first_global_row }` | Scanning sequentially from byte 0 or a validated same-generation checkpoint (`SourceStamp`, dialect, encoding match) through the snapped section end. | Shown (`first_global_row + local_row`) |
| `BoundaryTrust::Verified` | `QuoteFreeConfirmed` | Explicit user confirmation that the file contains no quoted fields (`quote = None`) **and** both snapped endpoints fall on encoding-aligned record terminators (or byte 0 / EOF). An arbitrary even UTF-16 offset alone proves nothing. | Unknown (*"global row unknown"*) |
| `BoundaryTrust::Speculative` | `SpeculativeAlignment` | Bounded (`<= 1 MiB`) alignment search using two-state LF speculation (`in_quote = false` vs `in_quote = true`). If no usable boundary exists within `1 MiB`, opening fails cleanly and recommends full indexing. | Unknown (*"global row unknown"*) |

### 4. Pausable, Cancellable Verify Boundary Job

- Any `Speculative` (or uncounted `QuoteFreeConfirmed`) section offers a cancellable, pausable background **Verify Boundary** job (`Session::start_verify_section_boundary`) that scans from the nearest validated checkpoint (or byte 0) through the section end under `SourceStamp` guards and reports honest throughput-based ETA.
- If verification confirms the same snapped interval, the section upgrades in place to `BoundaryTrust::Verified` with exact global row ordinals.
- If verification discovers that a multiline quoted field shifted the true record boundaries, the workbench never silently discards user drafts: clean sections reopen explicitly with the verified interval, while dirty sections preserve drafts for separate **Save Section As New File** export before explicit reopen.

### 5. Save Rules & Scope Boundary

- **Save Section As New File** is always available for both `Verified` and `Speculative` sections via Quarry's guarded atomic publisher (`ExportTarget::new_guarded`). On `Speculative` sections, the UI explicitly warns that the first record may be torn.
- In-place Save / Guarded Section Splice-Save is disabled in Phase 5A-2 and introduced in Phase 5A-4 (ADR 0005 amendment, `T13`), strictly requiring `BoundaryTrust::Verified`.

## Consequences

- Opening a bounded slice near 18 TiB in a 20 TiB APFS sparse file reads only the byte-0 header bootstrap and the `<= 1 MiB` alignment probe around the target window, indexing only the section's bytes via offset-translated positional `pread`.
- Section tabs never publish partial whole-file sidecars and keep their structural index scoped to `[snapped.base, snapped.end())` using absolute parent offsets in checkpoints.
- All downstream section features (5A-3 editing, 5A-4 Verified Splice-Save, 5A-5 Row Bookmarks) build on `SectionBounds`, `BoundaryTrust`, and `VerifyBoundaryJob` without weakening source-integrity guarantees.
