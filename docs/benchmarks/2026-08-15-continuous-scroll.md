# 12 GB continuous-scroll benchmark — 2026-08-15

## Scope

This run validates the first Phase 3 egui viewer slice: one continuous logical
scroll range over the entire file, with only the visible rows and two rows of
overscan materialized on either side. Environment and dataset match the
[engine benchmark](2026-08-14-large-file.md): an 11.33 GiB CSV containing
117,168,829 data rows on an Apple M3 Max MacBook Pro.

The release workspace was built with:

```bash
cargo build --workspace --release
```

The current `quarry-egui` binary was copied into a temporary local app bundle
for macOS accessibility control. `LARGE_FILE.csv` remains an untracked local
test file and is not part of the repository.

## Results

| Metric | Result |
|---|---:|
| Initial first rows, cache state unknown | 3,179.497 ms |
| Immediate warm reopen, first rows | 20.966 ms |
| Direct pointer drag to approximately 50% | Row 58,544,523 |
| Viewport read after the midpoint drag | 3.923 ms |
| Later near-end viewport read | 2.677 ms |
| Final viewport read | 1.395 ms |
| Resident memory after indexing and repeated jumps | 112.8 MiB |
| Animation Hitches trace | 0 hitches in 20.921 s |
| Potential interaction delays over 33 ms | 1 at 34.50 ms |
| Final visible range | Rows 117,168,815–117,168,829 |

The initial open was 179.497 ms above the PRD's three-second target. The warm
reopen was comfortably below it, but one uncontrolled-cache sample is not
enough to classify or optimize the difference. Controlled cold-cache repeats
remain a release-hardening task.

## Scroll and frame-pacing method

Computer Use performed a real pointer drag from the top to the middle of the
logical scrollbar, then additional navigation near the end. Apple Instruments
recorded the release app with the **Animation Hitches** template for 20.921
seconds while 23 scrollbar position changes and 20 Page Down actions exercised
the same navigation and viewport-read path.

Instruments' screen capture and Computer Use cannot capture the same window at
the same time, so the traced scrollbar changes used macOS accessibility value
updates rather than pointer drags. The real pointer drag was verified
immediately before the trace. Instruments reported no animation hitches and
one 34.50 ms potential interaction delay, just above its 33 ms reporting
threshold.

## Correctness and memory checks

- The scrollbar reached data row 117,168,829 without paging controls.
- The final visible record matched `tail -n 1 LARGE_FILE.csv` exactly.
- Viewport reads stayed below the existing 8 ms engine budget in the observed
  midpoint, near-end, and final positions.
- Resident memory stayed below the 500 MiB viewing ceiling after full indexing
  and repeated long-distance navigation.
- The UI buffer remains capped at the visible row count plus four overscan
  rows; no file-sized pixel range or row collection is allocated.

These measurements do not justify a larger overscan window or a new viewport
cache. Revisit either only if future pointer-drag traces show repeatable frame
hitches or viewport reads exceed the existing budget.
