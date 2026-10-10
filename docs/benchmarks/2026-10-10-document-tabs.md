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
| 151 | `e3e03aef14e70fe106c559eaef618be8b6fa736b` | `e34d8df0044e36806a2142074b035d48aa0ab5d29fe1dc3b9d204dd374cd30d6` | Active save/reload failure selects retained work; deferred split review respects concurrent-worker capacity |
| 153 | `0240d82f78a609bb59cdb32bdeca4cd0d527951a` | `5a317acec9f5fa1a69d76415d13b3ec84d268c4876cce7dbc027c3bd97824f47` | Native quit preserves idle Combine Files selections, running checks, and a combined-file write |
| 155 | `326cca7976a4bedb917c50a89a66c379dc5f1b46` | `3ff2c09f425c438ac8b39573a5f8039f6fc25c194d707bc9496b01ecd7aa533a` | Case-only Save As aliases rejected across tabs; retained edits and Undo/Redo |

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

Build 151 also reproduced the failure while the saved tab remained active. The
watcher moved a separate 10 GB clone after publication. Quarry automatically
selected B, retained its unsaved `VisibleB` header, displayed the saved-path error,
and removed the empty failed tab. B's Undo/Redo continued to work and its disk
checksum was unchanged. The moved file retained `ActiveRecoveryID,Name,Amount`.

## Deferred-operation resource regression

A completed split analysis can leave its materialization in an inactive tab's
storage review. Automated review found that this deferred operation could bypass
the global worker budget when resumed. Build 151 rechecks capacity before consuming
the review, including operations that resume directly after analysis.

The native test started split analysis on a separate synthetic 10 GB clone,
switched away, and ran a 10 GB sort plus a separate 10 GB save. Returning to the
pending split displayed **Two file operations are already running** with Continue
disabled. When the save completed, the warning cleared and Continue became
available. The split resumed, its close dialog showed **Applying column edit**,
and cancelling it closed only that tab. The separate saved output remained
available with `SecondDeferredID,Name,Amount`; the other sort could be cancelled
independently. The clone's source remained unchanged.

After both build-151 regressions, both original 10 GB inputs and the cancelled
split clone still matched their original SHA-256. A fresh streaming byte oracle
verified each additional saved output against the original with only its first
header field replaced:

| Output header | Output bytes | Verified SHA-256 |
|---|---:|---|
| `SecondDeferredID` | 10,395,000,164 | `c4a42ab53d9e8eaf9193b66388911ede5910ffcf2642a3749b525d542904ff86` |
| `ActiveRecoveryID` | 10,395,000,164 | `ae06951e27dc0ea69cbe5f81875539352cf9f1dd793e6799cb7375548b62dde1` |

## Quit with Combine Files open

Review found that native quit previously cancelled an active Combine Files job or
removed an idle dialog before any document quit approval. Build 153 blocks quit
while the dialog is open and explains that it must be closed explicitly. It keeps
file selections, format choices, checked plans, results and running operations.

In the native VM test, B retained an unsaved `QuitRetained` header. Command-Q kept
an idle Combine Files selection available and displayed **Close Combine Files
before quitting Quarry**. The dialog then checked two synthetic 10 GB files;
another Command-Q left the check running, and it completed with the same two
files, three columns and 110,000,018 data rows. A subsequent combined-file write
continued after Command-Q at 5% progress and published 20,790,000,285 bytes.
The completed result and close explanation remained visible. Explicitly closing
the dialog then allowed document quit review; Keep Editing retained B's edit and
Undo/Redo. Only the created synthetic edit was discarded to end the session.

Both original inputs again matched their SHA-256. An independent streaming
oracle concatenated the first input and the second input after its first header;
the entire output matched at SHA-256
`73fcdfb39b33f56896b0ac6d9c9cb4d834a7c7e4e571854662392ef4c7666b58`.

## Pending output aliases on macOS volumes

Review caught that two still-nonexistent outputs such as `out.csv` and `OUT.csv`
can name the same file on ordinary macOS storage. Build 155 checks the target
volume's case-sensitivity capability before comparing pending names. Canonically
equivalent Unicode names are also protected. Existing file identities remain
authoritative; an unavailable capability uses conservative case-insensitive
comparison for pending names.

The regression ran in VMPal on the guest's case-insensitive APFS volume and a
separate 64 MiB case-sensitive APFS disk image. An independent filesystem probe
confirmed each volume's behavior. The test reserved an output before publication,
rejected a case-only alias on the insensitive volume with no second save job or
created output, and allowed distinct names on the sensitive volume. It also
covered composed/decomposed Unicode and preserved meaningful accent differences.
The copied test binary's SHA-256 was
`f80233e625d583f98c4bcf13b3a5ebe2b766a07f0578828a1c51beb0a02d7bd6`;
its signature verified and both runs passed. The scratch image was detached.

The packaged app actually launched two documents. A 10 GB Save As to
`caseguard2.csv` completed while the other tab's request for `CASEGUARD2.csv`
was rejected with **This file is open or being written in another tab**. The
second tab retained `CaseRetainedB`, its original path, and Undo/Redo. The
before-publication condition is asserted by the regression above; the native
test verifies the picker, guard, and retained document state.

## Deterministic and CI validation

Formatting, strict workspace Clippy, 354 workspace tests (including 146 egui
tests), and release workspace builds passed locally. Regression coverage includes
per-tab drafts/selection/history, live independent filters, background save and
structural completion ownership, temporary-file cleanup, duplicate hard links,
reserved destinations, the two-operation limit, quit cancellation, text cursor
state, accessible keyboard interaction, compact visibility, and failed background
reload recovery, active-tab recovery, deferred-operation capacity checks, and
pending output aliases under both macOS volume case rules.
Multi-file drag-and-drop input is exercised by deterministic egui interaction
tests; native multi-file opening used Finder events and command-line
paths. The PR's latest commit must also pass required CI and CodeRabbit review
before merge.
