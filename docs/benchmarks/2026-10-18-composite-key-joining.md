# Composite-key join validation

Date: 2026-10-18. [#123](https://github.com/danchamorro/quarry/issues/123), part of [#71](https://github.com/danchamorro/quarry/issues/71).

## Automated verification

All 584 workspace tests, formatting, all-target/all-feature Clippy and the release build passed in the VMPal macOS VM (macOS 27.0.1, build 26A434).

The added engine checks cover:
- Independent component-by-component comparison against a nested-loop oracle, with exact output bytes and order across all four join modes and both case settings.
- Tuple-boundary collisions, embedded NULs, empty and missing components, whitespace, leading zeros, ASCII case differences, non-ASCII case differences and Unicode normalization differences.
- Matching sixteen ordered key pairs through analysis and file creation; rejecting zero, seventeen, unequal pair counts and out-of-range column selections.
- Identical case rules for uniqueness and duplicate expansion, including required acknowledgement for a newly case-folded many-to-many group.
- Comparison-key bounds before allocation, malformed framed-key rejection, bounded escaped duplicate labels, unchanged sources and owned spill cleanup.

The egui harness selects differently named columns, adds/removes/reorders pairs, changes the ASCII-only setting and checks the complete saved result. A small-window paint check verifies the footer's clipping bounds.

## Native workflow

Candidate build 173, clean source revision fe3d672dc5a99a8b375ce9313f6fd73c96d24f29, was packaged and operated in the same VM. The installed main application was not changed.

The Main file contained region/account/label and the Additional file used acct/area/city. Both were selected with native file pickers. Account was paired with acct, region with area, and the two pairs were reordered.

Exact matching reported two matching pairs and one unmatched Main row. After Reset Analysis and enabling Ignore ASCII letter case (A–Z only), analysis reported three matching pairs and no unmatched rows. The resulting 101-byte file matched a handwritten oracle, retained Main row order and original EU/eu field values, and left both input files unchanged. Open Joined File displayed three rows and six columns; the candidate then quit normally.

An earlier native candidate exposed clipped footer actions when the longer form grew. The scroll area now reserves space within the dialog for actions. The final native run verified that Reset Analysis, Choose Output and Create Joined File remain visible while the form and preview scroll.

## Scope

This adds one to sixteen key pairs and ASCII-only comparison to the saved, headered UTF-8 workflow. Output projection, other formats, working documents and the complete failure/large-file matrix remain in #124–#128. These checks do not establish the larger-than-RAM acceptance required by #128.
