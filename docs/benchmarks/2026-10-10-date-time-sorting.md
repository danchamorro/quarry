# Date and time sorting validation — 2026-10-10

Implementation: `98aaa4f4f48393d1054ac7a2cc853412905ee1c5`, main egui app
and shared core/validation CLI, [issue #56](https://github.com/danchamorro/quarry/issues/56).
Documentation-only follow-up fixes the PRD issue link and records this evidence.
No GPUI work or public binary release is included.

## Environment and build provenance

- VMPal VM `F5ED13E1-D535-4554-BA90-EAF680DB8420`, macOS 27.0.1 (26A434),
  arm64, 8 vCPUs, 16 GiB RAM. Host: Apple M3 Max.
- Rust 1.88.0, locked dependencies, optimized release build with workspace LTO.
- Local app 0.1.0, build 147, revision `98aaa4f4f48393d1054ac7a2cc853412905ee1c5`,
  `QuarrySourceStatus=clean`. Strict guest signature verification passed and
  the extracted app actually launched. Gatekeeper protections were not altered.
- ZIP SHA-256 matched on host and guest:
  `7b8963cc1d7093c42aeb959efdd566035ab1e1e2a001c373b9a09b5d919f5b70`.
- Guest CLI SHA-256:
  `f31bdaacbda5932036708d1eb6bd196f4518f441ec1a8a704f9824cab6d4d737`.
- All fixtures are synthetic. Shared host artifacts are under Downloads;
  extracted apps and guest fixtures use `/tmp/quarry-date-sort-review`.
  Existing guest work was preserved and the test app was closed afterward.

## Automated correctness

All **364 workspace tests** passed: 175 core, 148 egui, 31 CLI, 9 delimited,
and 1 AppKit. Formatting, strict workspace Clippy, and release builds passed.

New regressions cover explicit ambiguous-date interpretation, Gregorian year
and leap-century boundaries, exact nanosecond keys, offset normalization across
day/year boundaries, equal instants with different spelling, ASCII whitespace,
blank keys, exact widths, malformed/invalid bytes, and unsupported formats.
Forced tiny runs and two-way multipass merging verify stable ties in both
directions, header and cell edits, quoted/multiline whole rows, source retention,
and invalid-date cleanup after runs have already spilled. UI regressions verify
accessible format/direction controls, preserving the chosen format when the same
mode is selected again, Undo/Redo, and exact Save As output.

In the VM, all five formats in both directions matched **ten full byte oracles**
generated independently by Python 3.13.5's `datetime` and CSV writer. These
fixtures include years 0001 and 9999, 1900/2000 leap boundaries, ambiguous
month/day dates, blank cells, equal keys, Unicode, quoted commas/quotes, and
embedded newlines. The invalid 1900-02-29 fixture failed at data row 2, column 1,
with no output publication.

## Native VMPal workflow

- Opened month/day/year fixture; selected Date / time and MM/DD/YYYY. Oldest
  first placed March before April, retained equal-date row order, and kept the
  header and all other fields. Undo restored the original order; Redo restored
  chronology. Native Save published an exact ascending byte oracle.
- Reopened Sort, selected Newest first, and used the File dropdown's Save As.
  The published file matched the full descending oracle, while the prior source
  still matched its ascending oracle. The actual synthetic output name is
  `mdy-edited.csvnative-mdy-desc.csv` (the test's typing appended to the panel's
  default name); output identity and contents were checked explicitly.
- Opened the invalid fixture in another tab. Sort reported **data row 2,
  column 1, YYYY-MM-DD**; the grid and source were preserved. Corrected the date
  to 2026-03-05 without saving and successfully sorted newest first. Undo Sort
  retained the correction; a second Undo restored the original invalid value.
  The original source matched its full unchanged fixture afterward.
- The first saved tab remained intact throughout. Both tabs were clean before
  quitting. The review session left no test app process running.

VMPal's shortest click events were occasionally missed by the egui pickers;
explicit mouse-down/up with a 0.2-second hold produced reliable native input.
No application workaround or security setting was changed.

## Large-file resource and independent-output checks

`scripts/date-time-sort-fixture.py` generates 5,000,000 rows, **1,155,000,013
bytes** (1.08 GiB), with two offset spellings per equal UTC key, blank keys,
fractional seconds, stable ordinal IDs, Unicode, quotes, commas, and multiline
payloads. Python's `datetime` determines expected chronology; expected full
outputs are hashed in bounded batches, independently of Quarry's key parser.

```bash
python3 scripts/date-time-sort-fixture.py /NEW/fixture-directory --large-rows 5000000
quarry sort-save-as large.csv large-asc.csv --column 1 --order asc \
  --mode date --date-format rfc3339 --header first-row --cache-state warm
```

VM runs used `/usr/bin/time -l`. Cache was warm/mixed after copying and previous
reads; no cache flush was performed. Each result below is a single run. Text is
a same-build resource baseline, not a chronological-output baseline.

| Mode/direction | Sort wall time | Whole CLI time including validation | Peak CLI RSS | Peak temporary disk | Runs / merge passes |
|---|---:|---:|---:|---:|---:|
| Date/time oldest | 6.957 s | 13.46 s | 29.39 MiB | 2,488,125,013 bytes | 83 / 2 |
| Date/time newest | 6.896 s | 13.27 s | 29.34 MiB | 2,488,125,013 bytes | 83 / 2 |
| Text ascending | 6.597 s | 12.74 s | 28.19 MiB | 2,585,000,013 bytes | 89 / 2 |

Date/time's conservative allowance was 9,660,000,132 bytes. Both date sorts
preserved the full record count/header, verified stable source ordinals and
dual record fingerprints, and handed off a completed index with independently
checked first/middle/last samples. Full output SHA-256 matched the independent
oracle in both directions:

- Source: `8c6dc50423d01c292d96a82311ec7f3534e9d5881e212a0ac686234423e9c471`
- Oldest: `c109f69d7edf32200fc89c18428463641b823f1d1017ca5d75ee762ef9bd453d`
- Newest: `6c325913d22edaabe3af55ed973ca1a4bf3543b12d0a3df5ec1f87b1e746a298`

Cancellation requested at the 1 MiB threshold was observed at 2 MiB, stopped
after 3 MiB scanned, and joined in **0.654 ms**. No destination, sort run, or
export staging artifact remained. The source SHA-256 remained unchanged after
all runs. This fixture is smaller than the VM's RAM; these measurements do not
claim a new 50 GB date-sort or controlled cold-cache result. The sort continues
to use the previously validated bounded external run/merge architecture.
