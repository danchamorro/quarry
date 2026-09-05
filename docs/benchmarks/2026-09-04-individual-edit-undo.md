# Individual cell and header Undo validation: 2026-09-04

Priority 1 is implemented and locally validated on
`codex/individual-edit-undo`, based on
`73976e2aa827adf052ff9bcb95cfdf237e892369`. Initial validation used the
uncommitted feature build. Implementation commit `55ad8df` merged in
[PR #35](https://github.com/danchamorro/quarry/pull/35) as
`cd0501384d350e54850711ab7659fb2079f2cbce` on 2026-09-05. See the
[priority checklist](../PRE_BETA_CHECKLIST.md) for delivery status.

## Behavior and resource bounds

Committed cell values, header names, and Replace in Cell share sparse value
history. Each entry records one target and its previous and next overlay values.
Repeated edits, including restoring the underlying source value, remain separate
steps. Unchanged commits and cancelled editors preserve Redo. New actual edits
and successful whole-file operations invalidate obsolete Redo entries.

Individual history follows the existing adjacent structural snapshot. Undo
reverses later individual edits, the whole-file operation, and then earlier
individual edits; Redo follows the opposite order. Each retained indexed version
keeps at most 1,000 combined Undo/Redo entries and 16 MiB of before/after payload.
Oldest entries are evicted. An oversized entry clears history and is not retained;
untracked later sparse values block structural Undo. No CSV is copied per cell
edit. See the [user guide](../USER_GUIDE.md#undo-and-redo-changes) for lifecycle
and limit behavior.

## Automated validation

Validated on macOS 26.6.2 (25G83), arm64, Rust 1.88.0:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
git diff --check
```

All commands passed. The 256 workspace tests include 101 desktop tests, with
eight new focused regressions in `apps/quarry-egui/src/main.rs`. Rerun those with:

```bash
cargo test -p quarry-egui --locked individual_edit
```

The new cases cover:

- Mixed header/cell changes, repeated edits, multiline text, quotes, CRLF, BOM,
  source preservation, and returning to a clean overlay.
- No-op commits, Escape cancellation, new edit branches, and Replace in Cell.
- Undo and Redo across a structural boundary, including edits to derived cells
  and clearing obsolete individual Redo after a new structural operation.
- The entry cap, byte cap, oversized edits, and the structural cutoff.
- Save, Save As, Discard, and successful/failed document replacement, plus
  cancelled save pickers and write failure retaining history.
- External source changes detected by Undo, Redo, and Save without changing
  either the external file or the pending overlay.
- Accessible Undo/Redo controls, grid shortcuts, native cell/header typing
  Undo/Redo, and clearing native typing history between editor sessions.

Existing regressions also passed for cancellation, failed structural operations,
source conflicts, headerless files, sorting, row/column deletion, and persistence.
Independent implementation review found no actionable issues.

## CodeRabbit review: 2026-09-05

CodeRabbit CLI 0.7.5 completed this command with exit code 0:

```bash
coderabbit review --agent --uncommitted --include-untracked
```

The completion event reported `review_completed`, zero findings, and all seven
changed files reviewed: README, the desktop implementation, architecture, roadmap,
user guide, priority checklist, and this validation report. There were no reported
problems or optional suggestions to assess. File hashes confirmed that the review
left all seven files unchanged. Subsequent documentation updates record the
review and delivery status and clarify the history-limit instructions in the
guide and Undo tooltip. Application logic is unchanged; those wording updates
were made after this CodeRabbit run.

## Installed-app validation

Quarry initially had no file open. It was closed normally, then updated with
`./scripts/macos-app.sh install`. The installer verified and retained the prior
app in its normal local rollback archive. Both `./scripts/macos-app.sh verify`
and verification of a freshly packaged candidate passed. This is a local feature
build with `QuarrySourceStatus=dirty` and the base revision above, not a clean
release candidate. No bundles were published.

The installed `/Applications/Quarry.app` executable SHA-256 was:

```text
1bfd0537beb2f372171729d6d5fa037aedac3ffd1fee417d950f3288edcd1837
```

Using a disposable two-row CSV, the installed workflow verified:

1. Commit a multiline cell value containing a comma and quotes, rename its
   header, then edit that same cell again.
2. While typing, Command+Z restores the editor text and Command+Shift+Z reapplies
   it. Committing then enables document Undo.
3. Use the Undo button and Command+Z to reverse the repeated cell edit, header,
   and original cell edit separately until the document is clean. Redo restores
   each step.
4. Split the email column on `@`, then edit a derived cell from `b` to `bee`.
   Undo the derived edit, Split, and earlier header edit. Redo restores the
   header, Split, and derived edit in that order.
5. Save As to a new CSV. The saved document is clean, both history buttons are
   disabled, and exact byte comparison verifies the output and unchanged source.

The 28-byte source was `email,amount\r\na@b,1\r\nc@d,2\r\n`, with SHA-256
`b8ca883cd78b1f18c1cee3d64c444e4b49b213718da69b8a22129c4633d352e8`.
The 59-byte saved output was
`email,,total\r\na,bee,"line one\nline two,""quoted"""\r\nc,d,2\r\n`,
with SHA-256 `6c3f49fb3d43c99c2e692ad8e40c065922b5bcecebcd56c2fd87d6a088f3991f`.

This validates behavior and deterministic history bounds. It does not claim a
new large-file throughput or process-RSS benchmark. History payload limits do
not include the sparse overlay, allocator overhead, or temporary snapshot clones.

## Merged installation: 2026-09-05

After PR #35 merged, the local `main` checkout was updated to
`cd0501384d350e54850711ab7659fb2079f2cbce` and confirmed clean. The installer
updated `/Applications/Quarry.app` and preserved its verified rollback backup.
`./scripts/macos-app.sh verify` passed; the installed `QuarryGitRevision`
matched that merged commit and `QuarrySourceStatus` was `clean`. Cell and
header Undo/Redo passed again in the reopened installed app. This clean merged
installation supersedes the earlier dirty feature build for delivery status.
