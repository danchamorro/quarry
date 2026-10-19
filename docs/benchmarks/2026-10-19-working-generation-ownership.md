# Working-generation ownership regression evidence

Environment: VMPal macOS VM 1, macOS 27.0.1 (26A434), Apple Silicon;
Rust 1.88.0. All fixtures are synthetic. This is correctness and cleanup
evidence for the #126 foundation, not large-file performance or final GUI
acceptance evidence.

Before the fix, the real application regression
`storage::tests::working_generation_cleanup_never_follows_a_replaced_root`
failed: after a working directory was moved and its old name replaced with an
unrelated nested CSV, releasing the last generation lease recursively deleted
the replacement. Reading the unrelated CSV returned NotFound. The test now
creates its generation through the real private-writer publication handshake
and verifies both preservation of the replacement and cleanup of the actual
owned generation after the final lease.

Additional checks verify that:

- an actual private cleanup job retains its owner while awaiting acceptance,
  publishes into the captured moved directory, and reports its actual path;
- publication into a replacement of the captured owner directory is rejected;
- unregistered and individually substituted files survive cleanup, with errors
  retained beyond the final owner;
- a no-duplicate search releases its unused owned candidate even with existing
  working history (the added regression first found two retained files, not one);
- cancellation preserves a foreign entry at a requested destination and
  reports the nonempty private-directory cleanup failure;
- document disposal and pruning preserve generations until a captured reader
  finishes; existing structural Undo/Redo, source guards, no-clobber output,
  encoding, sorting, duplicate-removal and join regressions remain green.

The full combined workspace passed 642 tests. Formatting, all-target/all-feature
Clippy with warnings denied, a locked release workspace build, and all 17
macOS packaging self-tests passed. The Windows workflow now explicitly includes
the portable working-storage tests; a macOS pass does not establish that native
Windows result.

An earlier duplicate-preview test recreated a candidate with identical bytes
but a different file identity. Its old assertion required deletion of that
replacement. It now checks preservation and a cleanup diagnostic; the existing
unchanged-candidate cancellation tests continue to require owned-file removal.

Outstanding: desktop selection/capture and reservation guards for current
unsaved join inputs, cleanup coordination during close/quit, final native VMPal
acceptance, and the complete #127/#128 fault and scale matrices. No host
application work or installed-application update was performed for this change.
