# ADR 0012: Bounded access to compressed delimited files

- Status: Accepted (core engine, Slice 1; app and CLI, Slice 2)
- Date: 2026-11-20
- Issue: [#69](https://github.com/danchamorro/quarry/issues/69)

## Context

Large exports often arrive gzip-compressed. Today a user must decompress them
outside Quarry before opening them. Quarry's engine reads rows by seeking to
byte checkpoints in the source, and a deflate stream cannot be entered at an
arbitrary byte. Opening a compressed file therefore needs either a seekable
copy of the data or a decoder that can restart in the middle of the stream.

A compressed file also adds risks that an ordinary file does not have. A small
file can expand to fill the disk. A truncated download can look like a shorter,
valid file. A tar archive or a ZIP file can be mistaken for delimited text.
None of these may produce a document that silently differs from the source.

## Decision

Support gzip first, read-only. Quarry detects gzip by its signature
(`1f 8b 08`), not by its name. It decodes single-member and concatenated
multi-member streams (RFC 1952), including optional `FEXTRA`, `FNAME`,
`FCOMMENT` and `FHCRC` header fields. `FNAME` and `FCOMMENT` may each hold at
most 64 KiB, so a damaged header cannot make the reader scan the whole file for
its terminating NUL byte. The decoder is flate2 1.1.9 with its
pure-Rust `miniz_oxide` backend. Both crates and `crc32fast` were already in
`Cargo.lock` through image dependencies, so the lockfile gains no packages.

Opening has two steps.

1. `CompressedBootstrap` decompresses only as far as the first rows need,
   capped by the bootstrap limit, into memory. It writes nothing to disk. It
   detects the encoding, delimiter and header with the same code as an
   ordinary open. If the stream ends within that bound, the bootstrap verifies
   every trailer and rejects trailing bytes before it succeeds.
2. `CompressedDecompressJob` streams the whole source once, in 1 MiB
   decompressed chunks, into one owner-only file inside a private
   `SpillWorkspace` (directory `0o700`, file `0o600`, inherited macOS ACLs
   cleared, cleanup through the pinned directory handle). Each chunk is written
   to that file before the structural index records its rows, so live reads
   only return bytes already on disk. At the end the job rechecks the
   compressed source's version, syncs the copy, and returns a `Session` over
   the copy together with its completed index. No second indexing pass runs.

The `Session` keeps an `Arc` to the private copy. The last owner deletes it
and reports cleanup failures through `WorkingCopyCleanup`. Cancelling,
failing or dropping the job deletes the copy before the worker ends.
`reopen_compressed_backing` reinterprets the verified copy with another
delimiter, header choice or encoding without decompressing again.

### Why a streamed private copy

A deflate seek-point index, as in zlib's `zran.c`, stores a 32 KiB window
every few megabytes and restarts decompression from the nearest point. It
would save disk space, but every random read would decompress up to one
interval, and every existing engine (sort, filter, search, export, join,
inspection) would need a second, slower read path. A gzip file written as one
member also gives no cheap restart points. A private uncompressed copy lets
every existing operation run unchanged at the speed of an ordinary file, and
the single streaming pass already exists in the indexer. The cost is temporary
disk space equal to the decompressed size, which the bounds below control.

### Bounds

- `max_decompressed_bytes` caps the copy. The job checks the cap before it
  writes a chunk that would exceed it, then fails with
  `DecompressionLimitExceeded` and deletes the partial copy. The bootstrap
  applies the same cap to its in-memory bytes.
- Before starting, the job inspects the temporary folder and requires at least
  the compressed size in free space. While writing, it rechecks free space
  every `space_check_interval_bytes` (64 MiB by default). It stops with
  `InsufficientStorage` when free space falls below
  `min_free_space_reserve_bytes` (256 MiB by default). The first check happens
  only after 64 MiB, so small files work in tight test folders.
- The job checks for cancellation on every chunk. The gzip reader shares the
  job's cancellation flag and also checks it before each inflate step, at each
  member boundary, and every 4 KiB of a long header field.
- When a stream fills the bootstrap limit exactly, the bootstrap reads one
  more byte, which it never keeps, to learn whether the stream ended there.

### Strict integrity

The member reader drives flate2's `Decompress` directly instead of using
`MultiGzDecoder`. I checked flate2 1.1.9's `MultiGzDecoder` against each case
below. It rejects all of them, but truncation, a missing trailer and trailing
non-gzip bytes all surface as a generic `UnexpectedEof`, which a caller cannot
tell apart from a failing disk. The custom reader reports each case as
`CompressedFile` with a specific reason, and keeps real I/O errors as `Io`.

The reader fails on a truncated header, a truncated deflate stream, a corrupt
deflate payload, a CRC-32 mismatch, an ISIZE mismatch, a reserved header flag,
a bad header checksum, an unsupported method, a valid member followed by a
truncated or corrupt member, and any trailing bytes that are not another
complete member (zero padding included). ISIZE is compared modulo 2^32, so
members larger than 4 GiB verify. A damaged member can still emit wrong bytes
before its trailer fails. Those bytes only ever reach the private copy, which
is deleted with the failed job.

### Rejected inputs

Detection runs on every ordinary open as well, so `Session::open` never reads
compressed bytes as text. It rejects:

- tar archives, by `.tar.gz`/`.tgz` name or a `ustar` header in the first
  decompressed block;
- ZIP, Zstandard, bzip2 and XZ, by signature or extension, naming the format
  and stating that Quarry opens gzip (`.gz`);
- a `.gz` name without a gzip signature, as corrupt or renamed.

The bzip2 check also requires a block or end-of-stream magic after `BZh1`-`BZh9`,
because those four characters are ordinary text.

### Slice 1 contract

A compressed document is read-only. In-place Save and Save to Original reject
it with `CompressedReadOnly`. The core still accepts cell edits in Save Copy As,
but the app and CLI expose none (see Slice 2). Save Copy As and Export Filtered write uncompressed
delimited output. They reject a destination that names the compressed source
(`ExportDestinationIsSource`) or ends in `.gz` or `.tgz` (`InvalidOption`).
The rule applies to every writer (Save As, exports, Replace All, transforms,
sorting, duplicates, Combine Files, Join Files and recovery), because Quarry
never writes gzip. `is_compressed_destination` and
`reject_compressed_destination` expose it to the app and CLI. Inspection runs
on the private copy.

`CompressedRawDecompressJob` makes a private copy with the same validation,
bounds, cancellation, source check and cleanup, but does not parse it. It lets
inspection locate malformed delimited text inside valid gzip, which the
bootstrap would reject before any copy exists.
Combine Files and Join Files reject compressed inputs with a message that
says to save an uncompressed copy first.

### Slice 2: application and CLI

The app routes every `.gz` open through `CompressedBootstrap`.
`CompressedBootstrap::preview_session` gives the document a session over the
bootstrap rows that names no readable file, so the rows show before any
temporary file exists and nothing can read compressed bytes as text. The tab
then starts the job in the configured temporary-storage folder, reads rows past
the bootstrap from the job while it runs, and adopts the completed session and
index. Decompression counts against the two-operation limit of ADR 0007;
opening another compressed file while both slots are busy is refused before any
temporary file exists. Cancel, failure, tab close, and quit drop the job, which
deletes the copy; cancel and failure leave the bootstrap rows as a read-only
preview. Format changes call `reopen_compressed_backing`; Reload from Disk
decompresses the source again.

Compressed documents allow no edits of any kind. Save As and Save As UTF-8 are
available without edits, suggest the name without `.gz`, and open the
uncompressed result as an ordinary document.

The CLI's `open`, `inspect`, `viewport`, `search`, `filter`, `export`, and
`convert-utf8` accept `.gz` inputs with `--temp-dir` and
`--max-decompressed-bytes`; `open --no-wait` stops after the bootstrap. Ctrl-C
cancels the job, which removes the copy. Commands that change data reject
compressed inputs, and every command refuses `.gz` and `.tgz` output names.
`inspect` reads a gzip input through `CompressedRawDecompressJob`, so it
reports malformed text inside valid gzip. The app's Inspect File does the same
when valid gzip fails to open because of its text; damaged gzip is reported
without an inspection offer. The macOS
bundle declares `org.gnu.gnu-zip-archive` with the Viewer role.

## Alternatives considered

- Decompress with an external tool or a temporary file in the system folder:
  loses the private-file guarantees, the cap and cancellation.
- Keep the whole decompressed file in memory: violates the source-size
  independent memory contract.
- Seek-point index (`zran`): see above. Revisit only if measured disk use
  becomes the main complaint.
- Trust `MultiGzDecoder` as is: correct today, but its errors cannot drive
  useful messages and its member handling is not part of its documented
  contract.

## Deferred

- Zstandard (`.zst`) and other formats.
- Writing compressed output.
- Combine Files and Join Files over compressed inputs.

## Consequences and validation

Opening a compressed file costs one decompression pass and temporary disk
space equal to the decompressed size. In exchange every existing operation
works unchanged. Core tests cover every truncation point of single and
optional-header members, damage at every byte after the fixed header, CRC and
ISIZE mismatches, later-member damage and trailing bytes, rejected formats,
UTF-8, BOM, UTF-16 and Windows-1252 sources compared against the same data
opened uncompressed, live reads during decompression, cancellation and drop
cleanup, the decompressed cap, the free-space reserve, a source changed during
decompression, private permissions and cleanup after the private folder is
renamed, the read-only contract, exact Save Copy As and Export Filtered output,
and Combine and Join rejection. App and CLI tests cover the progressive open,
live reads, cancellation and failure previews, the read-only commands, Save As,
format reuse, the job limit, and cleanup on close and on Ctrl-C. Installed-app
checks run in the macOS VM.
