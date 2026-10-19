# Join input-format and ragged-width validation

Date: 2026-10-19. [#125](https://github.com/danchamorro/quarry/issues/125), part of [#71](https://github.com/danchamorro/quarry/issues/71).

## Automated verification

All 601 workspace tests, formatting, all-target/all-feature Clippy and the release
build passed inside VMPal (macOS 27.0.1, build 26A434).

The new independent fixture encoder and serializer cover all 25 pairs of UTF-8,
UTF-16 LE, UTF-16 BE, Windows-1252 and ISO-8859-1, with all four combinations of
source-header settings: 100 Full-mode cases. Main uses semicolons and Additional
uses tabs. Exact output bytes, decoded values, quoting, embedded CR/LF, default
header behavior, source preservation and owned-file cleanup are checked. Existing
all-mode, composite-key, duplicate-expansion and forced-spill regressions still run.

Additional checks cover mixed header-only sources, no-schema empty sources,
Unicode BOM detection, late key columns, stable appended output identities,
mandatory layout review, and schema recovery after output-column limits. The
UI harness corrects a legacy encoding after strict Auto analysis fails, configures
independent formats, joins composite keys, reviews new columns, or resets and
excludes them before reanalysis.

Malformed UTF-8, undefined Windows-1252 bytes, unpaired UTF-16 surrogates and odd
UTF-16 tails are rejected even when their field is excluded from output.
The new surrogate regression initially reported record 1 for malformed text in
record 2; using the existing structural inspection scanner plus per-record
decoding corrected the diagnostic. These checks verify the original source bytes
and the absence of owned unpublished data after failure.

## Native VMPal workflow

The release executable ran directly in the VM; installed bundles and the host
application were unchanged.

Binary SHA-256:

    9677f31e245e27c114587dd8784c36441c1431127fc06bcc8a83fe5b43e1b79b

Synthetic Main used a UTF-16 BE BOM, semicolons and no header:

    é;EU;Left
    é;US;Other;late

Additional used Windows-1252, tabs, and a header:

    acct    area    city
    é       EU      €

Native file pickers, the per-input header/encoding/delimiter controls and Add Key
Pair selected the first two columns as a composite key. Full analysis reported
two Main rows, one Additional row, one matching pair, two result rows and one
new Main column. The discovered column was selected and appended after the six
previously displayed output columns.

After selecting an output filename, clicking Create did nothing before layout
review; the retained analysis stayed visible and a filesystem check confirmed no
output existed. After checking the review acknowledgement, creation succeeded.

Exact UTF-8 output: 113 bytes, Main's semicolon delimiter, LF record separators,
with the header enabled because Additional has a header. SHA-256:

    ff939c667f43511f105fefa72518da40c30255657f312d226262dcdb640ad4f7

The result matched an independently constructed byte expectation. Both input files
retained their exact bytes. Open Joined File showed two data rows and seven
columns, preserving é, the euro symbol and the late value; the euro glyph was
confirmed visually after OCR omitted it. The synthetic session then quit normally.

## Boundaries

This ticket handles saved inputs. Current unsaved document capture and lifetime
leases remain #126. The complete fault/cancellation matrix and final
larger-than-RAM evidence remain #127 and #128; the parent feature is not complete.
