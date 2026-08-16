# Viewer file controls validation — 2026-08-15

## Scope

This Phase 3 slice adds native macOS file picking, one-file drag and drop,
delimiter overrides, header overrides, and explicit format reapplication to the
selected egui viewer. CLI paths and typed paths remain supported. The source
file is always opened read-only.

The viewport buffer remains capped at the visible rows plus two overscan rows
on each side. Opening also retains its bounded bootstrap rows. Opening and
reparsing use the existing bounded bootstrap and structural index. Replacing a
document starts the candidate index worker first so startup errors leave the
current document untouched. After startup succeeds, Quarry immediately cancels
and joins the prior worker before making the candidate current. Worker overlap
is limited to that handoff.

## Native picker decision

Quarry uses `rfd` 0.17.2 with default features disabled. On macOS it provides a
native `NSOpenPanel` and returns a `PathBuf`. It was chosen over direct AppKit
callbacks because it keeps the picker call small and avoids new unsafe UI glue;
its macOS support crates already match Quarry's objc2 dependency family. The
synchronous modal picker pauses egui repaint and visible progress updates while
it is open, although indexing continues in the background.

## Automated validation

The focused `quarry-core` and `quarry-egui` tests cover auto-detected comma data,
explicit tab, pipe, and semicolon delimiters, forced header presence and absence,
picker cancellation logic, failed-open preservation, one-file drop behavior,
explicit reopen, replacement during active indexing, synchronous worker
shutdown, malformed and empty files, later wider rows, and bounded continuous
scrolling.

All required checks passed on an Apple M3 Max MacBook Pro (16 CPU cores,
128 GiB RAM), macOS 26.6.1 (25G76), arm64, from base commit `f1a8f56`:

```text
cargo fmt --all --check                                      pass
cargo clippy --workspace --all-targets --all-features -- -D warnings
                                                            pass
cargo test --workspace                                      pass, 25 tests
cargo build --workspace --release                           pass
```

## Reference file

`LARGE_FILE.csv` is 12,167,847,982 bytes (11.33 GiB). It remains an untracked,
read-only local acceptance fixture and is not copied, regenerated, or committed.
Its modification time remained `2026-08-14 19:00:00 EDT` after validation.

## Computer Use acceptance

Codex Computer Use exercised a temporary local app bundle containing the release
binary. It launched Quarry without a CLI path, opened `comma.csv` with the native
picker, cancelled a second picker, and confirmed the comma document remained
unchanged. It also verified explicit tab parsing, forced header presence, forced
header absence with generated `Column N` labels, deferred settings until
**Apply / Reopen**, and malformed-file errors that preserved the prior valid
file. No crash, stale document, or visible lag appeared in these small-file
flows.

Finder drag and drop was not completed. The Computer Use runtime could not keep
a Finder source and Quarry destination visible in one cross-app drag. The
one-file and extra-file logic is covered by the egui regression test, but a real
Finder drop remains a human check.

## 12 GB results

The first native-picker run had **unknown cache state**. Computer Use first
observed usable rows after 18.763 seconds, when indexing had already reached
35.6%. This is an automation-bound upper limit, not a precise UI-ready time:
Quarry's in-app bounded-open metric was 7.108 ms, and Computer Use state capture
added multi-second delays. A later warm fresh-process launch reached the first
egui update in 169.852 ms. A warm engine run measured 8.193 ms to 100 rows and
20.891 seconds to index the file at 555.46 MiB/s. The final post-review build
measured 7.633 ms to 100 rows and 20.727 seconds at 559.86 MiB/s.

| Measure | Result |
|---|---:|
| Data rows indexed | 117,168,829 |
| Injected vertical scroll | Rows 1–16 to 28–43, 1.874 ms viewport read |
| Jump to data row 100,000,000 | Rows 100,000,000–100,000,015, 1.926 ms |
| Pointer drag to approximately 50% | Rows 58,659,881–58,659,896, 1.965 ms |
| Pointer drag to final viewport | Rows 117,168,814–117,168,829, 0.543 ms |
| Active-index replacement | 23.0% of the large index to `comma.csv` in 0.970 s |
| Live-index Page Down | Rows 1–16 to 17–32, 10.565 ms and 12.809 ms repeats |
| Post-index Page Down / Page Up | 6.408 ms / 2.882 ms |
| Final clean RSS after indexing | 136.1 MiB |
| Final clean macOS physical footprint | 154.6 MiB current, 398.2 MiB peak |
| Earlier long acceptance-session RSS | 182.0 MiB maximum sampled |
| Earlier long acceptance-session physical footprint | 254.7 MiB current, 517.5 MiB peak |

The final visible row matched `tail -n 1 LARGE_FILE.csv` exactly, including ID
`faa95ecd-da7c-4221-ac97-55318fd9afe6`, `DEBORAH WALKER`, the Shreveport
address, and final value `84414`. A real horizontal-scrollbar pointer drag moved
the row-number column offscreen and exposed `sequence_number`.

Computer Use also clicked **Apply / Reopen** and observed indexing restart. On
the corrected build it then observed the large file actively indexing at 23.0%,
entered the small-file path, and reached the complete three-row `comma.csv`
document 0.970 seconds later with no stale rows. A deterministic regression
starts a deliberately slow worker, confirms it is active, and replaces it; a
core regression separately proves that dropping an active job cancels and joins
its thread. An earlier run cancelled at 41.5%; the first 26 rows remained usable
and the status reported `Index cancelled`.

## Baseline comparison and limits

The final warm engine index time is 0.6% faster than the prior 20.862-second run
and remains within the earlier 20.703–21.521-second range. All observed
post-index and long-distance viewport reads remain below the 8 ms budget.
The 1.874–1.965 ms scroll, jump, and midpoint reads overlap the prior
1.395–3.923 ms range; the 0.543 ms final read is faster. Final clean post-index
RSS increased from 112.8 MiB to 136.1 MiB, and clean peak physical footprint
increased from 369.2 MiB to 398.2 MiB. Both remain below the 500 MiB viewing
target.

Two Page Down reads while the background indexer was active measured 10.565 ms
and 12.809 ms; the same keyboard path measured 6.408 ms after indexing, and Page
Up measured 2.882 ms. The accepted 8 ms engine target is explicitly post-index,
so this does not invalidate the cache decision. It does expose live contention:
the indexer held the structural-index write lock while scanning each 8 MiB
chunk, and the UI's snapshot waits for that lock. The follow-up
[live-index benchmark](2026-08-15-live-index-latency.md) measured snapshot
latency and index throughput together, then reduced the default chunk to 1 MiB.

An earlier, pre-review Computer Use session with repeated picker, reopen,
accessibility, and indexing operations reached 182.0 MiB sampled RSS and a
517.5 MiB physical-footprint high-water mark before settling to 254.7 MiB. The
final clean run did not reproduce that transient target overrun, but the full
long-session sequence was not repeated after the transactional handoff change
and removal of an unneeded index clone. The evidence does not isolate the
picker, accessibility harness, or repeated reopen as its cause. A controlled
base-versus-branch repeat remains necessary before classifying the 23.3 MiB
clean RSS increase or the long-session peak as a regression. No visible hitch
occurred in the successful scrolling, jumping, or pointer-drag run; this slice
did not repeat the prior Instruments Animation Hitches trace.

Computer Use verified Page Down and Page Up with the macOS `Next` and `Prior`
key names. It could not synthesize a physical trackpad gesture or complete the
Finder-to-Quarry drop, so those two interactions remain exact human checks.
Unit tests cover page movement, bounded scrolling, dropped-file selection,
active-worker replacement, and synchronous worker shutdown.
