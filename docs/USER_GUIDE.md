# Quarry User Guide

This guide explains how to open, inspect, edit, filter, reshape, sort, and save
delimited files in the Quarry macOS app.

## Contents

- [Getting started](#getting-started)
- [How to](#how-to)
  - [Navigate the grid](#navigate-the-grid)
  - [Select and copy data](#select-and-copy-data)
  - [Delete rows](#delete-rows)
  - [Find and remove duplicates](#find-and-remove-duplicates)
  - [Edit cells and headers](#edit-cells-and-headers)
  - [Undo and redo changes](#undo-and-redo-changes)
  - [Find and replace text](#find-and-replace-text)
  - [Filter rows](#filter-rows)
  - [Export filtered rows](#export-filtered-rows)
  - [Select columns](#select-columns)
  - [Split columns](#split-columns)
  - [Combine columns](#combine-columns)
  - [Move or delete columns](#move-or-delete-columns)
  - [Sort rows](#sort-rows)
  - [Manage the visible columns](#manage-the-visible-columns)
  - [Temporary storage and free space](#temporary-storage-and-free-space)
  - [Save, Save As, and discard](#save-save-as-and-discard)
- [Case matching](#case-matching)
- [Mouse and keyboard reference](#mouse-and-keyboard-reference)
- [Troubleshooting](#troubleshooting)

## Getting started

### 1. Install and open Quarry

If Quarry is not installed, follow [Install Quarry](../README.md#install-quarry)
in the README. The [macOS packaging guide](MACOS_PACKAGING.md) covers updates,
rollback, and package-only builds.

For prerelease testing, see the [beta checklist](BETA_RELEASE_CHECKLIST.md) for
platform status, the acceptance workflow, current limits, and the feedback form.

### 2. Open a file

Quarry supports comma, tab, pipe, and semicolon-delimited files. Open a file in
any of these ways:

- Double-click it in Finder after associating its file type with Quarry.
- Open Quarry's file menu, choose **Open…**, and select the file.
- Drop a local file onto the centered target shown when no file is open.

Quarry detects the delimiter and header row automatically. If the result is
wrong, open **Format**, choose the correct delimiter and header settings, then
click **Reopen with Changes**. You must save or discard unsaved changes before
reopening the file with different settings.

### 3. Understand the grid

- The top ruler contains the one-based file-column numbers for the current
  document. View-only hiding and reordering preserve those identities.
- The row below the ruler contains the column names when the file has a header.
- The numbers on the left identify data rows. The header is not counted as a
  data row.
- The file menu shows the current filename. A yellow dot marks unsaved changes,
  and the menu contains **Open…**, **Reload from Disk**, **Save**, **Save As…**,
  and **Discard Changes**.
- **Format** shows the applied delimiter and header mode. Opening it also shows
  what Quarry originally detected.
- The footer keeps the visible row range in view, adds file and selection
  metadata when space allows, and reserves its right side for status, progress,
  and the active operation's cancel button.
- Warnings and errors appear in a dismissible strip below the toolbar. Ordinary
  completion and informational messages stay in the footer.

Quarry displays the first rows before the complete file index is ready. You can
start reading and filtering immediately. Find actions and Sort become available
after indexing finishes.

### 4. Make a safe first edit

1. Double-click a cell.
2. Change its value and press Enter.
3. Confirm that the footer says **Modified (not saved)**.
4. Open the file menu, choose **Save As…**, and select a new filename.

This creates an edited copy and preserves the original file.

## How to

### Navigate the grid

- Scroll vertically with the mouse, trackpad, or the right-side scrollbar.
- Scroll horizontally to reach every shown column.
- Enter a one-based data-row number in **Row**, then click **Go** or press
  Enter.
- Click **Page Up** or **Page Down**, or use the keyboard Page Up and Page Down
  keys.

The number of visible rows follows the available window height. When a filter
is active, direct row jumping is disabled because the grid is showing matches
rather than a continuous source-row range.

### Select and copy data

- Click a cell to select it.
- Click a number on the left to select the whole displayed row.
- Press Command+C on macOS.
- Right-click a cell and choose **Copy** to copy its full decoded value,
  including embedded newlines.

A copied row is serialized as tab-separated text in the underlying file-column
order, including columns hidden in the current view. Copy is limited to 64 MiB.

### Delete rows

1. Click a numbered row gutter to select one data row.
2. Shift-click another row number to select a range.
3. Command-click on macOS, or Ctrl-click on other platforms, to add or remove
   separate rows.
4. Right-click a selected row number and choose **Delete Selected Rows**.

The row selection remains active while you scroll. Deletion creates an unsaved
working version; it does not change the source until you use **Save**. If the
storage allowance is at least 256 MiB, a preparation dialog checks space before
deletion starts. Click **Continue** once the check passes. Use **Undo** to restore
the previous working version, or **Discard Changes** to return to the last opened
or saved file.

Filtering clears the row selection. Clear an active filter before selecting or
deleting rows so every selected number identifies a physical data row.

### Edit cells and headers

#### Edit a cell

1. Double-click the cell, or select it and press Enter or F2.
2. Enter the new value. Use Shift+Enter to insert a newline.
3. Press Enter to keep the edit, or press Escape to cancel it.

Clicking elsewhere also keeps the active edit. The source file is not changed
until you choose **Save** from the file menu or press Command+S.

#### Rename a header

1. Click the header name below its numbered column ruler.
2. Enter the new name.
3. Press Enter or click elsewhere to keep it. Press Escape to cancel it.

Header editing is available only when the file has a real, text-decodable
header row.

### Undo and redo changes

Use the toolbar's **Undo** and **Redo** buttons to reverse and reapply committed
cell values and header names one at a time, including **Replace in Cell**.
Repeated changes to the same cell remain separate steps. Keeping an unchanged
value or cancelling an editor does not create a step or clear Redo.

On macOS, press Command+Z to undo and Command+Shift+Z to redo. Other platforms
use Ctrl+Z and Ctrl+Shift+Z, with Ctrl+Y also available for Redo. While typing
in a text field, these shortcuts act on that field's text. Commit or cancel an
inline cell or header editor before using document Undo or Redo. The commands
are unavailable while a filter or conflicting operation is active, or after a
source-file conflict.

Whole-file operations retain one adjacent working version. For example, after
editing a cell, sorting, and renaming a header, Undo restores the header, then
undoes the sort, then undoes the earlier cell edit. Redo follows the opposite
order. A new effective edit clears Redo. A second whole-file operation replaces
the older adjacent version, so this is not unlimited structural history.

Each retained document version has a limit of 1,000 combined Undo and Redo
entries and 16 MiB of before-and-after text. The oldest entries are dropped
when either limit would be exceeded. If one edit's before-and-after payload
exceeds 16 MiB, it clears that version's history and cannot be undone. History
limits can leave older unsaved values that block structural Undo. Save to keep
those changes or Discard Changes to restore the source; both reset history.
Individual edits store only the changed values, without copying the CSV.

Successful Save or Save As followed by reopening the saved file, Discard
Changes, and opening or reopening a document reset history. Cancellation or
failure keeps the current history while the current document remains open.

### Find and replace text

Find and Replace use literal text, not regular expressions.

1. Click **Find** in the toolbar, or press Command+F, to open the Find strip.
2. Enter a value in **Find (literal)**.
3. Leave **Match case** off to ignore ASCII letter case, or turn it on for an
   exact case match.
4. Click **Find Next**, or press Enter in the Find field. Quarry scrolls to,
   reveals, and highlights the matching cell.
5. After finding more than one match, use **Find Previous**, or press
   Shift+Enter in the Find field, to move backward.
6. To change the current match, click **Replace** to reveal the replacement
   row, enter text in **Replace with (literal)**, and click **Replace in Cell**.
7. To change every match, click **Replace All**.

**Replace in Cell** replaces every non-overlapping occurrence in the current
matching cell, then continues the search. It remains unavailable until Find has
selected a current match. **Replace All** scans the whole file and creates an
unsaved working version. Replace All changes data cells only and leaves the
header unchanged. Use the footer's **Cancel Search** or **Cancel Change** button
to stop a running operation. Use **Close find**, or Escape while a Find field
has focus, to return to the single toolbar row.

Find actions become available after indexing finishes. Find is disabled while
a filter is active. If the Find strip was already open, it remains visible but
disabled until you clear the filter.

### Filter rows

#### Filter from a cell

Right-click a cell and choose:

- **Filter to This Value** to show rows whose value equals the clicked value.
- **Filter Out This Value** to exclude rows whose value equals the clicked
  value.
- **Copy** to copy the value without filtering.

These actions use the full decoded cell value and inherit the current Filters
**Match case** setting. They start a new single-rule filter and replace any
active filter. Use **Filters…** when you need multiple rules.

#### Build one or more rules

1. Click **Filters…**.
2. Open the rule's **Column** picker. Search by name or original one-based
   file-column number, then choose a column. Each result shows its number and
   name, including columns hidden or reordered in the grid.
3. Choose a text rule (**Contains**, **Equals**, or **Does not equal**) or a
   numeric rule (**Greater than (>)**, **Greater than or equal (>=)**,
   **Less than (<)**, **Less than or equal (<=)**, or **Between (inclusive)**).
4. Enter the text value or numeric bound. Text values preserve spaces and line
   breaks. **Between** places its lower and upper bounds side by side and
   includes both endpoints.
5. Use **Add rule** for another condition.
6. Set **Match case** for text rules as needed, then click **Apply filters**.

Validation appears inside each rule. An empty **Contains** rule shows
**Enter text to match.** and keeps **Apply filters** disabled until text is
entered. Invalid numeric bounds also keep Apply disabled.

The rules scroll within the movable Filters window, keeping the bottom actions
in view. Expand **Details** for a summary of matching behavior and any applied
rules. **Close** preserves the draft and active filter. **Esc** first closes an
open picker; press it again to close Filters without applying or losing the
draft.

Filter rules work as follows:

- Every filtered column must match.
- Multiple inclusion rules for the same column are alternatives. This includes
  **Equals**, **Contains**, and all numeric rules. For example, State Equals
  `TX` and State Equals `FL` shows both states.
- Use one **Between** rule for a numeric range. Its lower and upper bounds must
  both match. Two separate same-column numeric rules are alternatives, so
  Greater than `100` plus Less than `1000` does not restrict rows to that range.
- Multiple **Does not equal** rules for the same column all apply. For example,
  State Does not equal `TX` and State Does not equal `FL` excludes both states.
- When inclusion and exclusion rules share a column, the value must match an
  inclusion and must not match any exclusion.
- **Contains** requires a value. **Equals** and **Does not equal** can compare
  against an empty cell.

Text filtering is case-insensitive by default. After applying rules, the toolbar
button changes to **Filters (N)…**, where N is the number of active rules. Open
it to inspect the rules, use **Clear filter** to return to all rows, or use the
footer's **Cancel filter** button while a scan is running.

Unsaved cell edits must be saved or discarded before filtering. Column
transformations that have already produced a working file can be filtered.

#### Numeric values and bounds

Numeric rules use the same exact interpretation as **Number** sorting, without
floating-point rounding. For example, `9007199254740993` is greater than
`9007199254740992`, and `2`, `02`, `2.00`, and `2e0` are equal numbers. Enter
`1000`, rather than `1,000`, for a thousand.

Signed dot decimals and scientific notation are accepted, with exponents from
`-1000000` through `1000000`. Leading and trailing ASCII whitespace is ignored.
Currency symbols, grouping separators, locale decimal commas, NaN, infinity,
and other nonnumeric text are not numbers.

Blank, missing, and invalid data values do not match a numeric rule; they do
not stop the scan. A text alternative on the same column can still match a
present blank or invalid value. Filter bounds must be valid, nonblank numbers,
and **Between** requires its lower bound to be no greater than its upper bound.
Quarry reports invalid bounds and disables **Apply filters** until they are
corrected. Editing a draft does not replace an active filter. **Match case** does
not affect numeric rules. Text **Equals** continues comparing literal text,
so use **Between** with equal bounds to match numerically equivalent values.

### Export filtered rows

After a filter finishes:

1. Open **Filters (N)…** and click **Export Filtered Rows…**.
2. Choose a new destination.
3. Wait for the export to finish, or click **Cancel Export** in the footer.

The export contains the header, when present, plus every matching row. It
writes a new file and does not replace the source. Save or discard unsaved
changes before exporting.

### Select columns

Column operations begin from the numbered ruler above the header names:

- Click a number to select one column.
- Shift-click another number to select a visible range.
- Command-click on macOS, or Ctrl-click on other platforms, to add or remove
  separate columns.
- Right-click a numbered ruler to open the column-operation menu. Right-clicking
  an unselected number selects that column first.

Selected columns are highlighted in the ruler and down the grid.

### Split columns

Use Split to replace one column with multiple columns based on an exact literal
separator.

1. Select exactly one numbered column.
2. Right-click its number and choose **Split Columns…**.
3. Enter a non-empty **Separator**, such as `@`, `,`, or `|`.
4. Click **OK**.

Quarry first scans the data to determine the required output width, then
returns the result to the ordinary editable grid. The result is unsaved until
you use **Save** or **Save As…**.

### Combine columns

Use Combine to replace two or more columns with one column.

1. Select at least two numbered columns.
2. Right-click a selected number and choose **Combine Columns…**.
3. Enter an optional literal **Separator**, such as a space, comma, or hyphen.
4. Click **OK**.

Values are combined in current document order. A blank separator places the
values directly next to each other. The completed result is an unsaved change.

### Move or delete columns

#### Move columns

1. Select one or more numbered columns.
2. Right-click a selected number and choose **Move Selected Columns…**.
3. Enter the one-based **Destination position** where the selected block should
   begin.
4. Click **Move**.

#### Delete columns

1. Select one or more numbered columns.
2. Right-click a selected number and choose **Delete Selected Columns**.

Delete has no separate deletion confirmation. If the storage allowance is at
least 256 MiB, a preparation dialog checks space before it starts. Click
**Continue** once the check passes. At least one column must remain. The source
is still unchanged until you save.
**Undo** first reverses later cell or header edits,
then restores the previous layout. **Discard Changes** restores the source
layout and removes every unsaved change.

For view-only reordering or hiding, use the **Columns…** window instead.

### Find and remove duplicates

1. Select the numbered columns that define a duplicate. Use Command-click to
   add columns or Shift-click for a range. Select every column to compare entire
   records.
2. Right-click a selected number and choose **Find Duplicates…**.
3. Check the selected-column summary. Choose **Match case** if uppercase and
   lowercase must match separately, then click **Find duplicates**.
4. Review **Extra duplicate rows** and **Rows to keep**. The extra count excludes
   the first occurrence of each matching group.
5. Click **Remove extra rows** to apply the result, or **Cancel** to keep the
   document unchanged.

The first-occurrence rule is shown while choosing options and reviewing the
result. Expand **Details** for matching and retained-row semantics. The review
also explains that removal creates an unsaved working version and can be undone.
Finding duplicates and removing them are separate actions; the review requires
an explicit **Remove extra rows** before the document changes.

Every selected column must match. Values are compared as decoded text bytes,
so quoted and unquoted forms of the same value match. Blank and missing fields
match each other. Spaces are significant and numeric-looking values remain
text, so `1`, `1.0`, and `␠1` differ. Here, `␠` represents one leading space.
Matching ignores ASCII letter case by default; non-ASCII bytes always compare
exactly.

Removal keeps the first occurrence in the current row order, including any
previous Sort, Reverse, or Shuffle. Retained rows keep all their cells and their
relative order. The header stays fixed. Current unsaved cell and header values
are included; duplicates do not require saving first. Clear any active filter
before starting.

Finding duplicates uses temporary disk files and prepares the result before
showing the count. Memory stays bounded as the file grows. Use **Cancel Change**
in the footer during the search. The review blocks editing until you remove or
cancel, so its count cannot become stale. Cancel, no duplicates, and failure
leave the document unchanged and clean up unpublished temporary files.

The applied result is an unsaved working version. **Undo** restores the previous
version and its edits; **Redo** reapplies removal. **Save**, **Save As…**, and
**Discard Changes** follow the usual document workflow. The source file is
unchanged until **Save**.

### Sort rows

Only Text, Number, Character count, and Word count use the selected column's
values. Shuffle and Reverse reorder all data rows and ignore the selected
column. The current **Sort Rows…** menu still requires one selected column to
open the window, including when choosing Shuffle or Reverse.

1. Select exactly one numbered column: the column to sort by for Text, Number,
   Character count, or Word count, or any column for Shuffle or Reverse.
2. Right-click its number and choose **Sort Rows…**.
3. Choose a type from **Sort as**:
   - **Text** (the default): alphabetical text order, such as `1, 10, 2`.
   - **Number**: exact numeric order, such as `1, 2, 10`.
   - **Character count**: shortest or longest values first, including spaces
     and embedded newlines. Counts Unicode scalar values, so a combining mark
     counts separately from its base letter. It does not count UTF-8 bytes.
   - **Word count**: fewest or most words first. A word is a nonempty group
     separated by Unicode whitespace, including tabs and newlines.
   - **Shuffle**: randomly reorder all data rows. Each use generates a fresh
     shuffle; equal-looking or duplicate rows remain separate records.
   - **Reverse**: put the last data row first and the first data row last.
4. Use the **Direction** buttons for Text, Number, Character count, or Word count.
   Shuffle and Reverse operate on the current whole-row order and ignore the
   selected column, so they have no direction control.
5. For Text, leave **Match case** off to ignore ASCII letter case, or turn it
   on for exact case ordering. This option applies only to Text.
6. Review the temporary-disk allowance, then click **Sort**.

The summary identifies the selected column by number and name. Hover a truncated
summary to read it in full. Expand **Details** for the selected mode's value
interpretation and ordering rules. The temporary-disk allowance stays above the
footer while the options scroll. **Cancel** closes the dialog without starting
a sort.

For a storage allowance of at least 256 MiB, **Preparing to sort…** checks
space automatically. Once **Ready to sort** confirms that enough disk space
is available, click **Continue**. The grid remains visible behind the dialog.
Expand **Storage details** to inspect the allowance or change the working folder.

Once sorting starts, the status bar moves through **Preparing to sort**,
**Sorting rows**, and **Merging sorted rows**. Quarry reuses the completed
index's row count instead of scanning the file again just to count rows. It
also builds the new row-location index while writing the sorted file, then
checks that the index still belongs to that file before using it. This lets
the sorted grid become ready without a separate indexing pass. If the filesystem
cannot provide the identity checks needed for reuse, Quarry falls back to indexing.
You can cancel during preparation as well as sorting and merging.

Character count and Word count require valid UTF-8 in the selected column.
Invalid text stops the operation with the data row and column identified.
Empty and missing cells count as zero; whitespace-only cells have zero words
but still have characters. Equal counts keep their current row order.

Number compares signed integers and dot decimals exactly, including values too
large for floating-point arithmetic. Scientific notation such as `1.25e3` is
accepted with exponents from -1,000,000 to 1,000,000. Surrounding ASCII whitespace
is ignored when comparing; original cell text is preserved. Empty, whitespace-only,
and missing values sort first ascending and last descending. Other text,
currency symbols, grouping commas, NaN, and infinity stop the sort with an error
identifying the data row and column, leaving the current document unchanged.

Every mode keeps the header fixed. Text, Number, Character count, and Word
count keep equal keys in their original order, including equivalent numbers
such as `2`, `02`, and `2.00`. Missing fields sort as empty cells. Large sorts can take time
and require substantial temporary disk space. During the merge phase,
**Merging sorted rows…** is a phase indicator rather than an exact completion
percentage.

The sorted grid is an unsaved working version. You can continue editing it,
undo the sort, save it, or discard it.

### Manage the visible columns

Click **Columns…** to change the view without changing the CSV structure:

- Use **Search columns** to find a column by name or original number.
- Uncheck a column to hide it, or check it to show it.
- Drag a column row using its handle or name to change its displayed position.
- Click **Reset** to restore the default visibility and order.
- Click **Done** or **Close**, or press **Esc**, to close the window.

The movable window keeps the search and footer actions visible while the
column list scrolls. Each row aligns its checkbox, original column number,
and name, including long names. Hover a truncated name to read it in full.
Closing preserves the search text and the current view settings.

These choices are view-only. They do not create an unsaved file change, and
they do not alter the order written by Save. Original file-column numbers stay
attached to their columns after a view reorder. Split, Combine, Move Selected
Columns, and Delete Selected Columns create a newly numbered working document.

Click **Auto-fit columns** at the bottom of the **Columns…** window to fit every
shown column to its header and the cell values already loaded into the grid.
Auto-fit works with any number of shown columns.

### Temporary storage and free space

Choose **File → Temporary storage…** to select a working folder on a drive
with enough free space. Use **Choose another folder…**, or enter an existing folder
and click **Check again**. **Use folder** applies the choice for this app
session. **Use system temporary folder** restores the default. Quarry checks
that the folder is available and can create, write, and remove a private file.

New working versions, sort runs, and duplicate-search runs use the chosen
folder. Changing the folder does not move or delete the current working version
or its Undo/Redo files. Keep their original drive connected. Quarry removes
versions when they become obsolete, and cleans up remaining private working
files on Save, Save As, Discard, document replacement, or normal shutdown.

Before an operation whose conservative storage allowance is at least 256 MiB,
a small preparation dialog checks space automatically. The grid remains visible
and lightly dimmed behind it; editing is temporarily blocked. A successful check
confirms **Enough disk space is available.** Click **Continue** to start the
operation, or **Cancel** to preserve the document. Expand **Storage details** to
see the working folder, estimated temporary space, available space, and retained
working/Undo files, or to choose another folder.

Large operations wait for indexing so the estimate uses the actual row count.
Split first checks the resulting column width without writing a working version.
If the check fails, the dialog explains the problem and opens the storage details.
Continue stays disabled until the check passes. Reconnect the drive, choose
another working folder, or free space, then check again.

The allowance covers new output, expanded fields, and simultaneous sort or
duplicate spill files. Retained versions already consume space on their own
volumes, so their sizes are shown separately rather than added twice to the
additional requirement. Estimates are conservative, not reservations. Other
applications can consume space after a check. Every output operation checks
again before writing, including operations below the review threshold.

**Save**, **Save As**, and filtered export stage their output beside the
destination, even when a different working folder is selected. This keeps
publication atomic. To use another destination volume, cancel and choose a
new **Save As** or export path. The original file and required Undo versions
remain available if a capacity check or a later write fails. Cancelled or failed
operations remove unpublished output; they never publish a partial file.

The CLI supports `--temp-dir DIRECTORY` for `sort-save-as` and `duplicates`.
This places scratch runs in the selected existing folder; count-only duplicates
also puts its private candidate there. Explicit output stays on its destination
volume. CLI edit, transformation, Save As, and export commands also check the
destination folder's capacity. The developer fixture generator is excluded.

### Save, Save As, and discard

The leftmost toolbar control reads **File** before a file is open and shows the
current filename afterward. Its menu contains **Open…**, **Reload from Disk**,
**Save**, **Save As…**, and **Discard Changes**. A yellow dot beside the filename
and **Modified (not saved)** in the footer identify unsaved work.

**Reload from Disk** rereads the current path with the applied format.
**Reopen with Changes** in the Format menu applies a confirmed delimiter or
header change instead.

- **Save** safely replaces the current file after the complete write succeeds.
- **Save As…** writes to a new unused path, preserves the previous source, and
  opens the saved copy after success.
- **Discard Changes** restores the last opened or saved file and removes all
  unsaved cell, header, Replace All, Split, Combine, Move Selected Columns,
  Delete Selected Columns, Delete Selected Rows, duplicate removal, and Sort changes.
- **Undo** and **Redo** reverse individual committed edits and move between
  adjacent whole-file working versions. See [Undo and redo changes](#undo-and-redo-changes)
  for history limits and reset behavior.

If you close a modified file, Quarry offers **Keep Editing**, **Save and
Close**, **Save As and Close…**, and **Discard Changes and Close**.

Quarry refuses to overwrite an existing Save As destination. It also detects
when the source changes outside Quarry and asks you to discard changes and
reopen it instead of overwriting the external update.

## Case matching

Find and Replace, text filters, Text sorting, and Find Duplicates each have an
independent **Match case** setting:

- Off, the default: ASCII uppercase and lowercase letters are treated as
  equivalent.
- On: ASCII letter case is compared exactly.

Cell context-menu filters inherit the Filters setting. The duplicate dialog
starts with **Match case** off each time. Search inside the
**Columns…** window is always case-insensitive. Split matches its separator
exactly, while Combine inserts its separator literally. Neither has a case
setting.

## Mouse and keyboard reference

| Action | Mouse or keyboard |
|---|---|
| Open a file | **Open…** in the file menu or empty-state target |
| Jump to a data row | Enter in **Row**, or click **Go** |
| Edit a cell | Double-click, or select and press Enter or F2 |
| Keep a cell edit | Enter or click elsewhere |
| Add a newline inside a cell | Shift+Enter |
| Cancel a cell or header edit | Escape |
| Rename a header | Click the header name |
| Undo a committed change | **Undo**, Command+Z on macOS, or Ctrl+Z elsewhere |
| Redo a committed change | **Redo**, Command+Shift+Z on macOS, or Ctrl+Shift+Z / Ctrl+Y elsewhere |
| Copy the selected cell or row | Command+C, or cell context-menu **Copy** |
| Save | Command+S, or file menu **Save** |
| Open Find | Command+F, or **Find** |
| Find the next match | Enter in Find, or **Find Next** |
| Return to a prior match | Shift+Enter in Find, or **Find Previous** |
| Close Find | **Close find**, or Escape while a Find field has focus |
| Move by one page | **Page Up**, **Page Down**, or the matching key |
| Select a row range | Shift-click numbered rows |
| Add or remove selected rows | Command-click or Ctrl-click numbered rows |
| Delete selected rows | Right-click a selected row number, then choose **Delete Selected Rows** |
| Find duplicate rows | Select numbered columns, right-click a selected number, then choose **Find Duplicates…** |
| Select a column range | Shift-click numbered columns |
| Add or remove selected columns | Command-click or Ctrl-click numbered columns |
| Open a focused context menu | Shift+F10 |

## Troubleshooting

| Problem | What to do |
|---|---|
| The delimiter or header is wrong | Open **Format**, choose the correct delimiter and header, then click **Reopen with Changes**. Save or discard changes first. |
| Find or Sort is unavailable | Wait for indexing to finish. To open Sort, select exactly one numbered column and right-click its number. Sort also needs the temporary-disk estimate. |
| Cell editing is unavailable | Clear active filters and wait for the current operation to finish. Missing and non-UTF-8 cells cannot be edited. |
| Undo or Redo is unavailable | Commit or cancel the inline editor, clear the filter, and wait for conflicting operations to finish. There may be no retained history in that direction. |
| Filtering is unavailable | Save or discard cell edits, then cancel or finish any active search, filter, export, or structural change. |
| A column operation is unavailable | Clear the filter, finish the active operation, and check that the required number of columns is selected. |
| **Delete Selected Rows** is unavailable | Clear the filter, finish the active operation, and select at least one numbered data row. |
| **Find Duplicates…** is unavailable | Clear the filter, finish the active operation, and select the numbered columns to compare. Accept or cancel an existing duplicate preview before editing. |
| A filter returns no rows | Check the original column number, value or numeric bounds, **Match case** for text rules, and same-column rule logic. Numeric rules skip blank, missing, and invalid values. |
| A numeric filter bound is rejected | Enter a nonblank dot decimal or scientific number without currency or grouping separators. Between needs two valid bounds in ascending order. |
| Find is disabled | A filter is active. Open **Filters (N)…** and clear it. |
| **Replace in Cell** is disabled | Use **Find Next** or **Find Previous** to establish the current matching cell first. |
| Quarry says the source changed | Use **Discard Changes**, then choose **Reload from Disk** from the file menu. |
| Save As will not use a path | Choose a destination that does not already exist. |
| A storage check fails | Reconnect the drive, choose an existing writable folder in File → Temporary storage, or free space and click Check space. For Save/export, choose another destination. |
| A dropped file does not open | Use one local file, and save or discard changes in the current file first. |
| A long operation is running | Read its progress or phase in the footer. Use the matching Cancel button there if needed. |
| A long operation appears stuck during sort | Check whether the status says **Merging sorted rows…**. This is a separate merge phase and can take substantial time on very large files. |

Cancelled or failed full-file operations do not publish a partial output file.
