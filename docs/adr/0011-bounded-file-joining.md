# ADR 0011: Bounded file joining

## Status and scope

Accepted implementation direction for [#71](https://github.com/danchamorro/quarry/issues/71), beginning with [#119](https://github.com/danchamorro/quarry/issues/119). This records the algorithm and resource obligations before extracting shared code. The join itself and its large-file acceptance remain unimplemented; existing sort measurements are not join measurements.

## Context

Joining two files can multiply records: a shared key with m rows on one side and n on the other produces m * n rows. Neither all keys, a duplicate group, nor all results can live in memory. The approved contract also requires source-row order, current working values, explicit analysis, private temporary storage, and atomic create-new publication. Sorting and duplicate removal already use bounded external runs, but their comparator and preservation verifier are operation-specific.

## Decision

Use external sort/merge on unambiguous framed comparison keys and original source ordinals. Prepare each source independently, sort its runs, analyze matching groups, and stream their matching pairs. Spool oversized duplicate groups to private disk and enumerate them in bounded blocks. Restore the approved output order through an external ordinal sort; key order must not escape into the result. No new database or sorting dependency is required for this direction.

Duplicate groups use byte ranges in the owned, immutable prepared runs as their disk-backed storage. The matcher counts and measures each group in one scan, then releases its key before enumerating rows. Creation holds a bounded Main block and streams the Additional range for that block; no file-sized or group-sized in-memory map is built. This avoids an extra copy of data already spooled privately. Ordering frames contain both Main and Additional ordinals so block iteration cannot change public row order.

The first extraction shares only private workspace ownership, the existing run frame, and bounded streaming frame I/O. Keep sort/duplicate comparison, numeric/date keys, fan-in calculations, progress, preservation checks and public APIs with their current operation. Preserve existing sort directory/file naming and the frame bytes. A generic reader must reject declared key or record lengths before allocating; callers supply their operation-specific limits and retain useful error context. Every frame body is consumed or skipped before another head is read.

### Memory

- Plan a maximum 512 MiB tracked working-payload budget per join. This is a ceiling, not a target allocation or a measured process-RSS promise. Buffer sizes and merge fan-in are derived from the remaining budget and checked before each phase; refuse a configuration that cannot fit its minimum live set rather than silently expanding the ceiling.
- Prepare and merge-sort the two inputs sequentially. During matching, read from completed sorted streams, not two concurrent multipass merge trees. Use bounded duplicate blocks and explicit limits for live keys, decoded records, output records, reader buffers, run buffers and previews.
- Preserve the 64 MiB serialized/decoded record bound and 65,536 output-column bound. Comparison-key framing adds bounded metadata for at most sixteen pairs; validate its total before allocation. A source-sized key does not justify keeping an unbounded duplicate group.
- Account for metadata and buffer capacity as well as logical lengths in implementation checks. Existing user-driven edit overlays and application view state are separate costs; share immutable payloads and keep snapshot preparation off the render thread. Two active operations must not be mistaken for one join's memory budget.
- Cancellation is checked during scans, spill/merge work, each bounded duplicate block, result generation, and before publication.

### Disk and analysis

Run a full cancellable analysis before creating a public result. Compute exact row cardinalities with checked arithmetic; for each matching group use checked m * n and accumulate all unmatched rows required by the selected join mode. Compute conservative serialized-byte allowances from actual group data, quoting, field separators, UTF-8 expansion, headers, and first-field BOM protection rather than a sampled average.

Use a per-volume allocation ledger. Include prepared snapshots, key and payload frames for both inputs, old/new merge generations, worst-live duplicate spools, ordinal-ordering generations, and final output staging. Do not count existing retained files again as new allocations; they already reduce available space. Release obsolete runs promptly and include allocations that coexist across phase transitions. Tests must compare observed peak temporary bytes to the advertised allowance. Recheck available space at start and before new phases; write/flush/sync failures remain possible after a successful check.

### Ownership, fidelity and publication

Keep logical-source, working-generation, format and overlay provenance with the analyzed plan. An Arc<Session> or copied path does not own a document's temporary directory; selected inputs need an explicit lifetime lease/reservation until the plan is consumed or released. Extend live application reservations and worker/quit handling before exposing the join.

Every workspace and spill file must be owner-only and clear inherited macOS ACLs before writing values. Reuse the same protection as #118. The final output is independently staged on its destination filesystem and published with create-new semantics only after source, output-count/schema, and cancellation checks. Report cleanup errors without deleting unrelated files. After atomic publication, cancellation retains the completed result.

For join and append-combine create-new outputs, capture the destination directory before checking its workspace membership. On Unix, retain a directory descriptor and use descriptor-relative private staging, no-replace publication and owned-file cleanup. A later symbolic-link retarget must neither redirect publication into a workspace nor strand its staging in the original folder. The membership check walks the captured directory's ancestry by identity. On Windows, retain non-delete-sharing handles for each resolved ancestor and reject changed/reparse-point components; no new Windows application work is implied.

A successful write resolves the captured directory's current name from its retained handle when the requested alias no longer names it (F_GETPATH on macOS and /proc/self/fd on Linux). It verifies that the resolved path still names that directory; an unsupported or unlocatable moved directory fails instead of reporting a stale path. Linux filesystems rejecting no-replace rename flags with EINVAL use descriptor-relative no-clobber hard links. Report post-publication staging cleanup failures alongside completion and retain the output. Cleanup removes only the owned data file and its empty staging directory; unrelated entries prevent directory removal and remain untouched. The same-filesystem private fallback for a macOS file-only drop folder remains supported. Existing Save As, source replacement and structural-edit publication constructors retain their established behavior.

Comparison normalization never changes emitted values. Validate UTF-8 explicitly because the existing decoder's UTF-8 branch intentionally returns raw bytes. Keep full-width source identities and output projection separate from the grid's visible columns. Sort's one-input record-multiset verifier is not a join verifier; verify pair cardinality, required unmatched records, projection, and the specified ordinal order instead.

## Alternatives

A file-sized in-memory hash table violates the source-size-independent memory contract. Partitioned disk hashing is viable but needs skew handling and an extra source-order restoration mechanism; the existing external-run approach offers more reusable, already tested infrastructure. A SQL engine adds dependency, collation, type-coercion and resource-policy decisions without removing the ordering and source-safety obligations. Neither alternative is selected. Revisit only if measured join acceptance fails, without silently changing product semantics.

## Validation and consequences

The extraction must leave existing sort and duplicate outputs, error categories, private naming, cancellation and cleanup unchanged. Run their existing regression coverage, forced small-budget/multipass tests, malformed frames and inherited-ACL checks. Frame-level tests cover exact byte compatibility, empty/truncated frames, oversize lengths before allocation, and bounded body handling.

Subsequent tickets prove joining separately with a small independent oracle and complete native workflows. Large-file acceptance must include data larger than RAM, a duplicate group larger than the join's in-memory block, long/composite keys, mostly unmatched data, and failure/cancellation in each phase. Record release-build RSS, peak temporary disk, elapsed time, cancellation latency and exact results; do not infer join performance from existing sort evidence.

### Cross-platform source and cleanup guards

Windows source versions capture the volume ID, 128-bit file ID and change time from the file handle, sampled around generic metadata capture. Path revalidation opens the named file as well as checking the retained handle. Same-size replacements and in-place rewrites with restored modification times must invalidate the selected schema. This extends the shared source-stamp contract used by join, combine and existing operations; Unix source-stamp behavior remains unchanged. Targeted Windows core CI exercises these guards, directory rename locking and nonrecursive output cleanup. It does not introduce a Windows application port.

Non-Unix pinned staging retains an owned directory path after disabling TempDir's recursive destructor. Only data.tmp and the empty owned directory may be removed, including on file-creation failure. Windows directory handles request directory read/attribute access with delete sharing denied; zero-access metadata handles do not establish the required rename exclusion.


### Retained spill directory identities

A retained preview owns a pinned spill directory, not authority to recursively remove its original name. Unix creates, opens and removes run files relative to that descriptor with no-follow/nonblocking safeguards; Windows retains the directory/ancestor delete-sharing locks. Every run reference carries the captured root and registered file identity. Cleanup removes only registered regular files and then the empty captured directory, including when its original name has been replaced. Foreign entries cause an explicit cleanup failure and remain untouched. There is no recursive fallback on release, cancellation, creation or Drop. Ordinary resolution first accepts the captured canonical path only while its leaf remains the same directory; a replaced leaf or moved directory requires a verified platform location. Unmoved Linux directories do not require procfs.

Private permission enforcement is required for all spill callers, including sort and duplicate removal. If a temporary volume cannot enforce owner-only access, fail before writing data and direct the user to another temporary folder. Every failed reservation attempts nonrecursive cleanup through its captured directory; preserve unrelated entries and report cleanup failure. Do not restore the older permissive behavior on such volumes.

The registry contains only live run identities: obsolete-run unlinking removes its entry. Join's existing bounded run-level count also bounds this metadata, and the aggregate reservation includes both displayed paths and identity/registry overhead. Destination membership uses the captured spill directory identity as well, so renaming it cannot allow output into owned scratch. Sorting and duplicate removal retain their frame format, naming, comparisons and result behavior while adopting the same storage ownership.

## Row-retention modes

The analysis configuration captures Left, Inner, Right or Full mode before a plan is retained. Counts of source matches and unmatched rows always describe both complete inputs; the projected output includes only rows retained by the chosen mode.

Matching pairs use Main then Additional ordinals for Left, Inner and Full, and Additional then Main ordinals for Right. Full appends Additional-only rows after all Main-driven rows. The private ordinal key uses the absent-side sentinel only where the mode permits it; publication validates both ordinal bounds and the retained-side rule. Preview uses the same ordering key as creation. Pair enumeration streams bounded blocks from the side driving that mode, so the preview's first pairs have the public order even when groups exceed the memory budget.

An absent source side serializes to empty fields without changing the order or width of either source's columns. Source comparison, full-input validation, uniqueness, duplicate acknowledgement, reservations and publication safeguards are unchanged.

## Composite comparison keys

One to sixteen ordered column pairs define a comparison tuple; both inputs must supply the same number of key columns. A nonempty tuple is framed as a matchable tag followed by a 64-bit length and UTF-8 bytes for each component. A tuple with any empty or missing component is represented by an unmatched tag plus its source ordinal. Boundaries and embedded NUL bytes cannot alias another tuple.

Case-sensitive comparison remains the default. ASCII-insensitive comparison lowercases only ASCII bytes in the private key; source records and result values remain unchanged. Private-key validation compares framed components against decoded source fields without allocating another full key. Duplicate samples escape controls and show bounded tuple components.

The complete framed key is capped at the existing record limit plus sixteen length prefixes and its tag. This is checked before allocation, including when a caller repeats the same source column in several key pairs. Resource accounting reserves the framing overhead and merge fan-in uses the observed complete key size.

## Result projection and headings

An output layout is an ordered list of source identities (Main or Additional,
original column index), independent of matching keys, names and grid order.
Default layouts include both sources in source order, including keys. The layout
is immutable with its analysis; selecting another source discards its UI draft,
and changing a retained layout requires releasing the analysis first.

Analysis computes exact serialized contributions only for selected fields while
still decoding and validating every input field. It accounts for first-field BOM
quoting at the selected output position, absent-side padding, separators and
optional header bytes. Creation applies the identical layout and validates the
actual selected width and byte total before publication.

Automatic headings retain unambiguous original names. Conflicts use the source
role and numeric suffixes; unnamed columns use the source role and original
column number. Explicit names are preserved and duplicate explicit names are
reported to the user. Requested names, collision bookkeeping and retained
headings share a bounded additional memory allowance. Preview and creation share
the resolved heading array rather than duplicating its payload.

The header switch is captured by the plan and completion result. Opening the
result passes the same header mode explicitly, so a header-free first record
remains data. The saved file always remains independent of source headers and
source edit history.
