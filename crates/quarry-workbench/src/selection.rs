//! Frontend-neutral grid selection, row selection, column view, and cursor state.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

/// Frontend-agnostic modifier flags for row and column selection gestures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SelectionModifiers {
    pub shift: bool,
    pub command: bool,
}

impl SelectionModifiers {
    pub const NONE: Self = Self {
        shift: false,
        command: false,
    };
    pub const SHIFT: Self = Self {
        shift: true,
        command: false,
    };
    pub const COMMAND: Self = Self {
        shift: false,
        command: true,
    };
}

/// Source-space `(row, column)` coordinate for grid cursor and cell targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellCoord {
    pub row: u64,
    pub column: usize,
}

impl CellCoord {
    pub const fn new(row: u64, column: usize) -> Self {
        Self { row, column }
    }
}

/// Selection shape tracked by [`GridCursor`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CursorSelectionMode {
    /// Single active cell (or no active selection when `active` is `None`).
    #[default]
    Single,
    /// Whole source row selected at `active.row`.
    Row,
    /// Rectangular anchor-to-active cell range (`anchor`..=`active`).
    Range,
}

/// UI-neutral grid cursor and selection model (position and selection state only).
///
/// Arrow-key navigation, focus rings, and Vim motions are wired in frontends on top
/// of this model in later phases.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GridCursor {
    pub active: Option<CellCoord>,
    pub anchor: Option<CellCoord>,
    pub mode: CursorSelectionMode,
}

impl GridCursor {
    pub fn clear(&mut self) {
        self.active = None;
        self.anchor = None;
        self.mode = CursorSelectionMode::Single;
    }

    pub fn active(&self) -> Option<CellCoord> {
        self.active
    }

    pub fn anchor(&self) -> Option<CellCoord> {
        self.anchor
    }

    /// Moves the cursor to `coord`, either starting a fresh single-cell selection
    /// or extending a rectangular range from the current anchor.
    pub fn move_to(&mut self, coord: CellCoord, extend: bool) {
        if extend {
            let anchor = self.anchor.or(self.active).unwrap_or(coord);
            self.anchor = Some(anchor);
            self.active = Some(coord);
            self.mode = if anchor == coord {
                CursorSelectionMode::Single
            } else {
                CursorSelectionMode::Range
            };
        } else {
            self.active = Some(coord);
            self.anchor = Some(coord);
            self.mode = CursorSelectionMode::Single;
        }
    }

    /// Places the cursor on a full-row selection while retaining column 0 as reference.
    pub fn select_row(&mut self, row: u64) {
        let column = self.active.map_or(0, |coord| coord.column);
        let coord = CellCoord::new(row, column);
        self.active = Some(coord);
        self.anchor = Some(coord);
        self.mode = CursorSelectionMode::Row;
    }

    /// Returns normalized inclusive row bounds when a cursor is active.
    pub fn row_bounds(&self) -> Option<RangeInclusive<u64>> {
        let active = self.active?;
        let anchor = match self.mode {
            CursorSelectionMode::Range => self.anchor.unwrap_or(active),
            CursorSelectionMode::Single | CursorSelectionMode::Row => active,
        };
        Some(anchor.row.min(active.row)..=anchor.row.max(active.row))
    }

    /// Returns normalized inclusive source column bounds when in cell or range mode.
    pub fn column_bounds(&self) -> Option<RangeInclusive<usize>> {
        let active = self.active?;
        match self.mode {
            CursorSelectionMode::Row => None,
            CursorSelectionMode::Single => Some(active.column..=active.column),
            CursorSelectionMode::Range => {
                let anchor = self.anchor.unwrap_or(active);
                Some(anchor.column.min(active.column)..=anchor.column.max(active.column))
            }
        }
    }

    /// Converts the active cursor position into a [`GridSelection`] for legacy callers.
    pub fn as_grid_selection(&self) -> Option<GridSelection> {
        let active = self.active?;
        Some(match self.mode {
            CursorSelectionMode::Row => GridSelection::Row { row: active.row },
            CursorSelectionMode::Single | CursorSelectionMode::Range => GridSelection::Cell {
                row: active.row,
                column: active.column,
            },
        })
    }

    /// Synchronizes the cursor state from an existing [`GridSelection`].
    pub fn sync_from_grid_selection(&mut self, selection: Option<GridSelection>) {
        match selection {
            None => self.clear(),
            Some(GridSelection::Row { row }) => self.select_row(row),
            Some(GridSelection::Cell { row, column }) => {
                self.move_to(CellCoord::new(row, column), false);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridSelection {
    Cell { row: u64, column: usize },
    Row { row: u64 },
}

impl GridSelection {
    pub fn row(self) -> u64 {
        match self {
            Self::Cell { row, .. } | Self::Row { row, .. } => row,
        }
    }

    pub fn selects_row(self, row: u64) -> bool {
        matches!(self, Self::Row { row: selected, .. } if selected == row)
    }

    pub fn selects_cell(self, row: u64, column: usize) -> bool {
        self.selects_row(row)
            || matches!(
                self,
                Self::Cell {
                    row: selected_row,
                    column: selected_column,
                    ..
                } if selected_row == row && selected_column == column
            )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowSelection {
    pub ranges: Vec<RangeInclusive<u64>>,
    pub anchor: Option<u64>,
}

impl RowSelection {
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn contains(&self, row: u64) -> bool {
        let index = self.ranges.partition_point(|range| *range.end() < row);
        self.ranges
            .get(index)
            .is_some_and(|range| range.contains(&row))
    }

    pub fn count(&self) -> u64 {
        self.ranges.iter().fold(0_u64, |count, range| {
            count.saturating_add(range.end().saturating_sub(*range.start()).saturating_add(1))
        })
    }

    pub fn first(&self) -> Option<u64> {
        self.ranges.first().map(|range| *range.start())
    }

    pub fn clear(&mut self) {
        self.ranges.clear();
        self.anchor = None;
    }

    pub fn select(&mut self, row: u64, modifiers: SelectionModifiers) {
        if modifiers.shift {
            let anchor = self
                .anchor
                .or_else(|| self.ranges.first().map(|range| *range.start()))
                .unwrap_or(row);
            self.ranges.clear();
            self.ranges.push(anchor.min(row)..=anchor.max(row));
            self.anchor = Some(anchor);
        } else if modifiers.command {
            self.toggle(row);
        } else {
            self.ranges.clear();
            self.ranges.push(row..=row);
            self.anchor = Some(row);
        }
    }

    pub fn toggle(&mut self, row: u64) {
        let index = self.ranges.partition_point(|range| *range.end() < row);
        if self
            .ranges
            .get(index)
            .is_some_and(|range| range.contains(&row))
        {
            let start = *self.ranges[index].start();
            let end = *self.ranges[index].end();
            match (row == start, row == end) {
                (true, true) => {
                    self.ranges.remove(index);
                }
                (true, false) => self.ranges[index] = row.saturating_add(1)..=end,
                (false, true) => self.ranges[index] = start..=row.saturating_sub(1),
                (false, false) => {
                    self.ranges[index] = start..=row.saturating_sub(1);
                    self.ranges.insert(index + 1, row.saturating_add(1)..=end);
                }
            }
            if self.anchor == Some(row) {
                self.anchor = self.ranges.first().map(|range| *range.start());
            }
            return;
        }

        self.ranges.insert(index, row..=row);
        self.merge_adjacent();
        self.anchor = Some(row);
    }

    fn merge_adjacent(&mut self) {
        let mut merged: Vec<RangeInclusive<u64>> = Vec::with_capacity(self.ranges.len());
        for range in self.ranges.drain(..) {
            if let Some(previous) = merged.last_mut()
                && *range.start() <= previous.end().saturating_add(1)
            {
                let start = *previous.start();
                let end = (*previous.end()).max(*range.end());
                *previous = start..=end;
            } else {
                merged.push(range);
            }
        }
        self.ranges = merged;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnView {
    pub order: Vec<usize>,
    pub hidden: Vec<bool>,
    pub shown: usize,
    pub start: usize,
    pub visible: Vec<usize>,
}

impl ColumnView {
    pub fn new(total_columns: usize) -> Self {
        let mut view = Self {
            order: (0..total_columns).collect(),
            hidden: vec![false; total_columns],
            shown: total_columns,
            start: 0,
            visible: Vec::new(),
        };
        view.refresh();
        view
    }

    pub fn shown_count(&self) -> usize {
        self.shown
    }

    pub fn extend_to(&mut self, total_columns: usize) {
        let current = self.hidden.len();
        if total_columns <= current {
            return;
        }
        self.order.extend(current..total_columns);
        self.hidden.resize(total_columns, false);
        self.shown = self.shown.saturating_add(total_columns - current);
        self.refresh();
    }

    pub fn view(&mut self, column: usize) -> bool {
        if column >= self.hidden.len() {
            return false;
        }
        if self.hidden[column] {
            self.hidden[column] = false;
            self.shown = self.shown.saturating_add(1);
        }
        self.refresh();
        true
    }

    pub fn set_shown(&mut self, column: usize, shown: bool) -> bool {
        let Some(hidden) = self.hidden.get_mut(column) else {
            return false;
        };
        let was_shown = !*hidden;
        if was_shown != shown {
            self.shown = if shown {
                self.shown.saturating_add(1)
            } else {
                self.shown.saturating_sub(1)
            };
        }
        *hidden = !shown;
        self.refresh();
        true
    }

    pub fn move_column(&mut self, column: usize, target: usize) -> bool {
        let Some(position) = self.order.iter().position(|source| *source == column) else {
            return false;
        };
        if target >= self.order.len() {
            return false;
        }
        if position != target {
            let column = self.order.remove(position);
            self.order.insert(target, column);
        }
        self.refresh();
        true
    }

    pub fn reset(&mut self) {
        for (column, source) in self.order.iter_mut().enumerate() {
            *source = column;
        }
        self.hidden.fill(false);
        self.shown = self.hidden.len();
        self.refresh();
    }

    pub fn refresh(&mut self) {
        self.start = 0;
        // Render every shown column; add horizontal virtualization if very-wide files become measurably slow.
        self.visible = self
            .order
            .iter()
            .copied()
            .filter(|source| !self.hidden[*source])
            .collect();
    }
}

pub fn selected_split_column(selected_columns: &BTreeSet<usize>) -> Option<usize> {
    if selected_columns.len() == 1 {
        selected_columns.iter().next().copied()
    } else {
        None
    }
}

pub fn validated_column_selection(
    columns: Vec<usize>,
    total_columns: usize,
) -> Result<BTreeSet<usize>, String> {
    if columns.is_empty() {
        return Err("Select at least one numbered column first.".into());
    }
    if columns.iter().any(|column| *column >= total_columns) {
        return Err("A selected column is outside this file.".into());
    }
    let selected = columns.iter().copied().collect::<BTreeSet<_>>();
    if selected.len() != columns.len() {
        return Err("Select each column only once.".into());
    }
    Ok(selected)
}

pub fn select_column(
    columns: &ColumnView,
    selected_columns: &mut BTreeSet<usize>,
    anchor: &mut Option<usize>,
    column: usize,
    modifiers: SelectionModifiers,
) {
    if modifiers.shift {
        let shown_columns = columns
            .order
            .iter()
            .copied()
            .filter(|shown| !columns.hidden[*shown])
            .collect::<Vec<_>>();
        let anchor_column = (*anchor)
            .filter(|anchor| shown_columns.contains(anchor))
            .or_else(|| {
                shown_columns
                    .iter()
                    .copied()
                    .find(|shown| selected_columns.contains(shown))
            })
            .unwrap_or(column);
        selected_columns.clear();
        let Some(anchor_position) = shown_columns
            .iter()
            .position(|shown| *shown == anchor_column)
        else {
            selected_columns.insert(column);
            *anchor = Some(column);
            return;
        };
        let Some(column_position) = shown_columns.iter().position(|shown| *shown == column) else {
            selected_columns.insert(column);
            *anchor = Some(column);
            return;
        };
        let range = anchor_position.min(column_position)..=anchor_position.max(column_position);
        selected_columns.extend(shown_columns[range].iter().copied());
        *anchor = Some(anchor_column);
    } else if modifiers.command {
        if selected_columns.remove(&column) {
            if *anchor == Some(column) {
                *anchor = selected_columns.iter().next().copied();
            }
        } else {
            selected_columns.insert(column);
            *anchor = Some(column);
        }
    } else {
        selected_columns.clear();
        selected_columns.insert(column);
        *anchor = Some(column);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_view_hides_reorders_resets_and_keeps_every_shown_column() {
        let mut columns = ColumnView::new(100);
        assert_eq!(columns.shown_count(), 100);
        assert_eq!(columns.visible.len(), 100);
        assert_eq!(columns.visible[0], 0);
        assert_eq!(columns.visible[99], 99);

        assert!(columns.set_shown(1, false));
        assert_eq!(columns.shown_count(), 99);
        assert!(!columns.visible.contains(&1));

        assert!(columns.move_column(95, 0));
        assert_eq!(columns.visible[0], 95);

        assert!(columns.view(1));
        assert_eq!(columns.shown_count(), 100);
        assert!(columns.visible.contains(&1));

        columns.reset();
        assert_eq!(columns.shown_count(), 100);
        assert_eq!(columns.visible[0], 0);
        assert_eq!(columns.visible[99], 99);
    }

    #[test]
    fn row_selection_supports_single_shift_range_and_command_toggle() {
        let mut selection = RowSelection::default();
        assert!(selection.is_empty());

        selection.select(5, SelectionModifiers::NONE);
        assert_eq!(selection.ranges, vec![5..=5]);
        assert_eq!(selection.count(), 1);
        assert!(selection.contains(5));

        selection.select(2, SelectionModifiers::SHIFT);
        assert_eq!(selection.ranges, vec![2..=5]);
        assert_eq!(selection.count(), 4);
        assert_eq!(selection.anchor, Some(5));

        selection.select(3, SelectionModifiers::COMMAND);
        assert_eq!(selection.ranges, vec![2..=2, 4..=5]);
        assert_eq!(selection.count(), 3);
        assert!(!selection.contains(3));

        selection.select(3, SelectionModifiers::COMMAND);
        assert_eq!(selection.ranges, vec![2..=5]);
        assert_eq!(selection.count(), 4);

        selection.clear();
        assert!(selection.is_empty());
        assert_eq!(selection.first(), None);
    }

    #[test]
    fn grid_cursor_tracks_single_row_and_rectangular_range_state() {
        let mut cursor = GridCursor::default();
        assert_eq!(cursor.active(), None);
        assert_eq!(cursor.as_grid_selection(), None);

        cursor.move_to(CellCoord::new(10, 3), false);
        assert_eq!(cursor.mode, CursorSelectionMode::Single);
        assert_eq!(cursor.row_bounds(), Some(10..=10));
        assert_eq!(cursor.column_bounds(), Some(3..=3));
        assert_eq!(
            cursor.as_grid_selection(),
            Some(GridSelection::Cell { row: 10, column: 3 })
        );

        cursor.move_to(CellCoord::new(4, 7), true);
        assert_eq!(cursor.mode, CursorSelectionMode::Range);
        assert_eq!(cursor.anchor(), Some(CellCoord::new(10, 3)));
        assert_eq!(cursor.active(), Some(CellCoord::new(4, 7)));
        assert_eq!(cursor.row_bounds(), Some(4..=10));
        assert_eq!(cursor.column_bounds(), Some(3..=7));

        cursor.select_row(12);
        assert_eq!(cursor.mode, CursorSelectionMode::Row);
        assert_eq!(cursor.row_bounds(), Some(12..=12));
        assert_eq!(cursor.column_bounds(), None);
        assert_eq!(
            cursor.as_grid_selection(),
            Some(GridSelection::Row { row: 12 })
        );

        cursor.sync_from_grid_selection(Some(GridSelection::Cell { row: 2, column: 1 }));
        assert_eq!(cursor.mode, CursorSelectionMode::Single);
        assert_eq!(cursor.active(), Some(CellCoord::new(2, 1)));

        cursor.clear();
        assert_eq!(cursor.active(), None);
        assert_eq!(cursor.anchor(), None);
    }
}
