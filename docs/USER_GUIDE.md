# Quarry User Guide

This guide explains how to open, inspect, edit, filter, reshape, sort, and save
delimited files in the Quarry macOS app.

## Contents

- [Getting started](#getting-started)
- [How to](#how-to)
  - [Navigate the grid](#navigate-the-grid)
  - [Select and copy data](#select-and-copy-data)
  - [Edit cells and headers](#edit-cells-and-headers)
  - [Find and replace text](#find-and-replace-text)
  - [Filter rows](#filter-rows)
  - [Export filtered rows](#export-filtered-rows)
  - [Select columns](#select-columns)
  - [Split columns](#split-columns)
  - [Combine columns](#combine-columns)
  - [Move or delete columns](#move-or-delete-columns)
  - [Sort rows](#sort-rows)
  - [Manage the visible columns](#manage-the-visible-columns)
  - [Save, Save As, and discard](#save-save-as-and-discard)
- [Case matching](#case-matching)
- [Mouse and keyboard reference](#mouse-and-keyboard-reference)
- [Troubleshooting](#troubleshooting)

## Getting started

### 1. Install and open Quarry

If Quarry is not installed, follow [Install Quarry](../README.md#install-quarry)
in the README. The [macOS packaging guide](MACOS_PACKAGING.md) covers updates,
rollback, and package-only builds.

### 2. Open a file

Quarry supports comma, tab, pipe, and semicolon-delimited files. Open a file in
any of these ways:

- Double-click it in Finder after associating its file type with Quarry.
- Click **Choose…** and select the file.
- Drag a local file into the Quarry window.
- Enter its full path in **File**, then click **Open** or press Enter.

Quarry detects the delimiter and header row automatically. If the result is
wrong, choose the correct **Delimiter** or **Header** setting, then click
**Apply / Reopen**. You must save or discard unsaved changes before reopening
the file with different settings.

### 3. Understand the grid

- The top ruler contains the one-based file-column numbers for the current
  document. View-only hiding and reordering preserve those identities.
- The row below the ruler contains the column names when the file has a header.
- The numbers on the left identify data rows. The header is not counted as a
  data row.
- The footer reports file size, delimiter, header mode, indexed rows, the
  visible row and column ranges, and the current selection.

Quarry displays the first rows before the complete file index is ready. You can
start reading and filtering immediately. Find and Sort become available after
indexing finishes.

### 4. Make a safe first edit

1. Double-click a cell.
2. Change its value and press Enter.
3. Confirm that the footer says **Modified (not saved)**.
4. Click **Save As…** and choose a new filename.

This creates an edited copy and preserves the original file.

## How to

### Navigate the grid

- Scroll vertically with the mouse, trackpad, or the right-side scrollbar.
- Scroll horizontally to reach every shown column.
- Enter a one-based row number in **Data row**, then click **Jump** or press
  Enter.
- Use **Page Up** and **Page Down**, or the keyboard Page Up and Page Down keys.

The number of visible rows follows the available window height. When a filter
is active, direct row jumping is disabled because the grid is showing matches
rather than a continuous source-row range.

### Select and copy data

- Click a cell to select it.
- Click a number on the left to select the whole displayed row.
- Click **Copy** or press Command+C on macOS.
- Right-click a cell and choose **Copy** to copy its full decoded value,
  including embedded newlines.

A copied row is serialized as tab-separated text in the underlying file-column
order, including columns hidden in the current view. Copy is limited to 64 MiB.

### Edit cells and headers

#### Edit a cell

1. Double-click the cell, or select it and press Enter or F2.
2. Enter the new value. Use Shift+Enter to insert a newline.
3. Press Enter to keep the edit, or press Escape to cancel it.

Clicking elsewhere also keeps the active edit. The source file is not changed
until you use **Save**.

#### Rename a header

1. Click the header name below its numbered column ruler.
2. Enter the new name.
3. Press Enter or click elsewhere to keep it. Press Escape to cancel it.

Header editing is available only when the file has a real, text-decodable
header row.

### Find and replace text

Find and Replace use literal text, not regular expressions.

1. Enter a value in **Find (literal)**.
2. Leave **Match case** off to ignore ASCII letter case, or turn it on for an
   exact case match.
3. Click **Find Next**. Quarry scrolls to and reveals the next matching cell
   without changing the current selection.
4. To change that cell, enter text in **Replace with (literal)** and click
   **Replace in Cell**.
5. To change every match, click **Replace All**.

**Replace in Cell** replaces every non-overlapping occurrence in the current
matching cell, then continues the search. **Replace All** scans the whole file
and creates an unsaved working version. Replace All changes data cells only and
leaves the header unchanged. Use **Cancel Search** or **Cancel Change** to stop
a running operation.

Find becomes available after indexing finishes. Find and Replace are hidden
while a filter is active, so clear the filter before searching.

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
2. Enter the original one-based file-column number for the rule.
3. Choose **Contains**, **Equals**, or **Does not equal**.
4. Enter the literal value.
5. Use **Add rule** for another condition.
6. Set **Match case** as needed, then click **Apply filters**.

Filter rules work as follows:

- Every filtered column must match.
- Multiple **Equals** or **Contains** rules for the same column are
  alternatives. For example, State Equals `TX` and State Equals `FL` shows
  both states.
- Multiple **Does not equal** rules for the same column all apply. For example,
  State Does not equal `TX` and State Does not equal `FL` excludes both states.
- When inclusion and exclusion rules share a column, the value must match an
  inclusion and must not match any exclusion.
- **Contains** requires a value. **Equals** and **Does not equal** can compare
  against an empty cell.

Filtering is case-insensitive by default. Click **Filters active…** to inspect
the current rules, use **Clear filter** to return to all rows, or use **Cancel
filter** while a scan is running.

Unsaved cell edits must be saved or discarded before filtering. Column
transformations that have already produced a working file can be filtered.

### Export filtered rows

After a filter finishes:

1. Click **Export Filtered Rows…**.
2. Choose a new destination.
3. Wait for the export to finish, or click **Cancel Export**.

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

Delete begins immediately after validation and does not show another
confirmation dialog. At least one column must remain. The source is still
unchanged until you save. **Undo Change** can restore the previous layout unless
later cell or header edits are unsaved; **Discard Changes** restores the source
layout and removes every unsaved change.

For view-only reordering or hiding, use the **Columns…** window instead.

### Sort rows

1. Select exactly one numbered column.
2. Right-click its number and choose **Sort Rows…**.
3. Choose **Ascending** or **Descending**.
4. Leave **Match case** off to sort ASCII letters without separating uppercase
   and lowercase values, or turn it on for exact case ordering.
5. Review the temporary-disk allowance, then click **Sort**.

Sort keeps the header fixed and is stable, so equal values retain their
original order. Missing fields sort as empty cells. Large sorts can take time
and require substantial temporary disk space. During the merge phase,
**Merging sorted rows…** is a phase indicator rather than an exact completion
percentage.

The sorted grid is an unsaved working version. You can continue editing it,
undo the sort, save it, or discard it.

### Manage the visible columns

Click **Columns…** to change the view without changing the CSV structure:

- Use **Search columns** to find a column by name or original number.
- Uncheck a column to hide it, or check it to show it.
- Click and drag a column row to change its displayed position.
- Click **Reset** to restore the default visibility and order.
- Click **Done** to close the window.

These choices are view-only. They do not create an unsaved file change, and
they do not alter the order written by Save. Original file-column numbers stay
attached to their columns after a view reorder. Split, Combine, Move, and Delete
create a newly numbered working document.

Click **Auto-fit columns** to fit every shown column to its header and the cell
values already loaded into the grid. Auto-fit is available when 64 or fewer
columns are shown.

### Save, Save As, and discard

- **Save** safely replaces the current file after the complete write succeeds.
- **Save As…** writes to a new unused path, preserves the previous source, and
  opens the saved copy after success.
- **Discard Changes** restores the last opened or saved file and removes all
  unsaved cell, header, Replace All, Split, Combine, Move, Delete, and Sort
  changes.
- **Undo Change** and **Redo Change** move one step between completed whole-file
  working versions. They are not a per-cell edit history, and they are
  unavailable while later cell or header edits remain unsaved.

If you close a modified file, Quarry offers **Keep Editing**, **Save and
Close**, **Save As and Close…**, and **Discard Changes and Close**.

Quarry refuses to overwrite an existing Save As destination. It also detects
when the source changes outside Quarry and asks you to discard changes and
reopen it instead of overwriting the external update.

## Case matching

Find and Replace, Filters, and Sort each have an independent **Match case**
setting:

- Off, the default: ASCII uppercase and lowercase letters are treated as
  equivalent.
- On: ASCII letter case is compared exactly.

Cell context-menu filters inherit the Filters setting. Search inside the
**Columns…** window is always case-insensitive. Split matches its separator
exactly, while Combine inserts its separator literally. Neither has a case
setting.

## Mouse and keyboard reference

| Action | Mouse or keyboard |
|---|---|
| Open a typed file path | Enter in **File** |
| Jump to a data row | Enter in **Data row** |
| Edit a cell | Double-click, or select and press Enter or F2 |
| Keep a cell edit | Enter or click elsewhere |
| Add a newline inside a cell | Shift+Enter |
| Cancel a cell or header edit | Escape |
| Rename a header | Click the header name |
| Copy the selected cell or row | Command+C, or **Copy** |
| Save | Command+S, or **Save** |
| Move by one page | Page Up or Page Down |
| Select a column range | Shift-click numbered columns |
| Add or remove selected columns | Command-click or Ctrl-click numbered columns |
| Open a focused context menu | Shift+F10 |

## Troubleshooting

| Problem | What to do |
|---|---|
| The delimiter or header is wrong | Choose the correct **Delimiter** or **Header**, then click **Apply / Reopen**. Save or discard changes first. |
| Find or Sort is unavailable | Wait for indexing to finish. Sort also needs the temporary-disk estimate. |
| Cell editing is unavailable | Clear active filters and wait for the current operation to finish. Missing and non-UTF-8 cells cannot be edited. |
| Filtering is unavailable | Save or discard cell edits, then cancel or finish any active search, filter, export, or structural change. |
| A column operation is unavailable | Clear the filter, finish the active operation, and check that the required number of columns is selected. |
| **Auto-fit columns** is unavailable | Hide columns until 64 or fewer are shown. |
| A filter returns no rows | Check the original column number, value, **Match case** setting, and same-column rule logic. |
| Find and Replace disappeared | A filter is active. Clear it to restore Find and Replace. |
| Quarry says the source changed | Use **Discard Changes**, then reopen the externally changed file. |
| Save As will not use a path | Choose a destination that does not already exist. |
| A long operation appears stuck during sort | Check whether the status says **Merging sorted rows…**. This is a separate merge phase and can take substantial time on very large files. |

Cancelled or failed full-file operations do not publish a partial output file.
