# Independent document tabs: macOS VM validation

## Environment and build provenance

The production egui app was tested through VMPal in an Apple Silicon macOS VM:
macOS 27.0.1 (26A434), eight virtual CPUs, 16 GiB allocated RAM, arm64, VMPal
Tools 0.37. The host toolchain was Rust 1.88.0. Native interaction used VMPal
input, observation, screenshots, and guest execution. No host desktop review
testing or Gatekeeper bypass was used. Bundles stayed local; this is not evidence
of a newly published or notarized release.

All packages were release builds, verified with `codesign --verify --deep --strict`.
Host and guest ZIP checksums matched. Each launched bundle reported clean source.

| Build | Git revision | ZIP SHA-256 | Coverage |
|---|---|---|---|
| 146 | `043461a250de81289507e03630d03449d1f548b9` | `ee4d28558914be6557ae8b4c900bfe9fbdcc0278b939d7d3faab81bad8720895` | Independent editing, Undo/Redo, Save As, close, Finder events, quit cancellation, minimum window |
| 147 | `cdbced25223eef3c0c28891b341087923bc27a57` | `b5686ad8ee39b48f7b3702e3caed20acb7eb891eb97b54824cd4a5c3d6e062a4` | Eight tabs, keyboard wrap, ninth-open rejection; exposed resize visibility regression |
| 148 | `31bec964c118de3c5e4f7a3665e8170e64bc221b` | `4c3c6aef202ff170d864177991ace9c4e6145f54af75a0419864d937120c448f` | Resize regression fixed; large files, concurrent file operations, cancellation and resource sampling |
| 149 | `d80e7fd9f73e7feee7f5f199fdc1e210fb981d40` | `5a7b0e1953ae75e295535ff312d0247bba1f6f76bf9a24be732ea3a14caf997b` | Background save/reload failure recovery; other tab's edits and history retained |

Each build actually launched and rendered its test documents. Documentation-only
commits after these builds do not change the tested executable code.

## Native interaction results

- Opening two command-line paths created two tabs. Multi-file Finder open events
  created more tabs while the existing document had unsaved edits. Opening an
  existing path selected its tab instead of duplicating it.
- Cell edits in B remained intact while A's header edit was undone and redone.
  Save As moved A to its edited copy; B remained modified and retained its value.
- Command-W prompted for only the active modified file. Keep Editing preserved it.
- Quit first reviewed modified D. Discard was approved, then quit was cancelled at
  B's prompt. All four tabs remained; D still displayed `d-edited`, B retained its
  edit, and Undo/Redo remained available. The earlier discard approval did not
  dispose of D.
- Eight files opened successfully. A ninth Finder request produced an actionable
  limit warning without eviction. Control-Tab and Control-Shift-Tab wrapped through
  the collection and revealed the selected tab.
- A real window resize to the 860 × 540 content minimum exposed an offscreen active
  tab in build 147. Build 148 retained the active eighth tab in view after that same
  resize; the toolbar and grid remained usable.
- A 10 GB sort and a separate 10 GB Save As ran together. Command-W on the sort tab
  showed **Finishing file operation**, its path and live progress. **Cancel Operation**
  cancelled the sort and closed only that clean tab. The other save completed and
  its tab adopted the saved filename. A separate unsaved 1 GB structural edit
  remained available in its own tab.

## Data and resource evidence

All inputs were synthetic. The CSV profile has three columns (`ID,Name,Amount`),
quoted comma-containing text padded with ASCII characters, and LF records.
Each 1 GB input has 5,500,000 data rows. Each larger input repeats the complete
1 GB fixture ten times: 55,000,009 data rows, including nine repeated header
records treated as ordinary data after the first header.

| Original inputs | Bytes each | Original SHA-256, unchanged after testing |
|---|---:|---|
| Two 1 GB CSVs | 1,039,500,015 | `7dd556bf8958d231ff31fbbabcaaf4d375087af3f12b56be8284200b3c7357eb` |
| Two 10 GB CSVs | 10,395,000,150 | `c0e5360aaf5a119b151d381f202b9c9c456daae60a20c1d084d8dd8226eaf1fd` |

Four large documents were open together: 22,869,000,330 bytes (21.3 GiB), exceeding
the VM's 16 GiB RAM allocation. Process RSS sampled every approximately 100 ms
plus `ps` overhead peaked at **160,064 KiB (156.3 MiB)** across 2,619 samples over
388.9 seconds, covering viewing, indexing, saves, sorts and cancellation. This
measures app RSS, not kernel filesystem cache or total machine memory.

The filesystem cache was not flushed; earlier checksum reads and previous fixture
use warmed some data, and the aggregate corpus exceeded RAM. This is a correctness
and observed-resource run, not a controlled cold/warm throughput comparison or an
aggregate memory promise for all eight tabs and every operation. Before this
feature, the app could retain only one document; there is no equivalent baseline
for a simultaneous multi-document session. Core scan algorithms were unchanged.

An independent streaming SHA-256 oracle substituted only the requested first
header field and compared every remaining byte against the original. All four
saved outputs matched exactly:

| Output header | Output bytes | Verified SHA-256 |
|---|---:|---|
| `SourceOneID` | 1,039,500,024 | `1a25d65d0f2d9166f2c2c78588af2f1ecb7297e36428e2b66e2eb57dc4500b30` |
| `SourceTwoID` | 1,039,500,024 | `8e16de80815e8472c08596102b655af115bd307bfae2a92324288c7bf88fc8ad` |
| `SecondSaveID` | 1,039,500,025 | `cf55e28585066d5db56057c06d92a544b2cc6c6ced1b3cdfce979e8114d76c9e` |
| `SourceBigTwoID` | 10,395,000,162 | `7c7cee35120b6559cab7c508dece6fdaf821c18266c863c8f84400bf3ec495d4` |

The small fixture sources also retained their initial checksums after editing,
Save As and cancelled quit. Native tests were ended by discarding only explicitly
created synthetic edits; existing guest work was preserved.

## Background save recovery regression

Automated review identified that a successful background in-place save followed
by a failed reopen could leave an unreachable empty tab and hide its error.
Build 149 removes that empty tab and surfaces the saved path/reopen error on the
visible tab while preserving that tab's document and history.

The native test cloned a synthetic 10 GB fixture to a separate test path. A guest
watcher moved that clone immediately after atomic save publication, forcing its
background reopen to fail. The remaining B tab visibly retained `retained-b` and
displayed **Saved … but could not reload it … Reopen the file to continue**. The
empty failed tab was absent. B's Undo and Redo still worked. The moved saved file
remained present at 10,395,000,159 bytes with header `RecoveredID,Name,Amount`;
the original source still had `ID,Name,Amount`, and B's disk checksum was unchanged.

## Deterministic and CI validation

Formatting, strict workspace Clippy, 352 workspace tests (including 144 egui
tests), and release workspace builds passed locally. Regression coverage includes
per-tab drafts/selection/history, live independent filters, background save and
structural completion ownership, temporary-file cleanup, duplicate hard links,
reserved destinations, the two-operation limit, quit cancellation, text cursor
state, accessible keyboard interaction, compact visibility, and failed background
reload recovery, active-tab recovery, and deferred-operation capacity checks. Multi-file drag-and-drop input is exercised by deterministic egui
interaction tests; native multi-file opening used Finder events and command-line
paths. The PR's latest commit must also pass required CI and CodeRabbit review
before merge.
