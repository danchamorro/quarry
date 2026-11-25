//! Session-only row bookmarks keyed by `(source_generation, record_start_offset, global_ordinal)`.
//!
//! Bookmarks target logical CSV data records across whole files and both section trust modes
//! (`Verified` and `Speculative`), survive inline cell edits, active filters, and tab dormancy,
//! shift deterministically across row insertions and deletions, clear with notice on row-order
//! changes (restorable via Undo), and feed Phase 7 Split-at-Bookmarks.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Bound, RangeInclusive};

/// Fixed per-document maximum row bookmark count (session-only; aligns with Phase 7A-1's
/// 10,000-part file-splitter cap).
pub const MAX_ROW_BOOKMARKS: usize = 10_000;

/// Stable logical-record bookmark key `(source_generation, record_start_offset, global_ordinal)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BookmarkKey {
    /// Monotonically increasing generation of the document's active backing file.
    pub source_generation: u64,
    /// Byte offset where the logical CSV record begins in the active backing file.
    /// `None` only transiently after a row-preserving/shifting working-copy rewrite while the
    /// new generation is still re-indexing.
    pub record_start_offset: Option<u64>,
    /// 1-based global data row ordinal when proven by a counted index/checkpoint, or `None`
    /// when global row ordinals are unknown (e.g., `Speculative` or uncounted `QuoteFreeConfirmed`
    /// sections).
    pub global_ordinal: Option<u64>,
}

/// One bookmarked logical CSV data record within a document or section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowBookmark {
    /// Zero-based local physical record index within the active document/section (`>= data_start`).
    pub local_row: u64,
    /// Stable `(source_generation, record_start_offset, global_ordinal)` identity key.
    pub key: BookmarkKey,
}

/// Verified, deduplicated bookmark target ready for Phase 7 Split-at-Bookmarks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookmarkSplitTarget {
    pub source_generation: u64,
    pub local_row: u64,
    pub record_start_offset: u64,
    pub global_ordinal: Option<u64>,
}

/// Deterministic bookmark lifecycle transformation applied when a new backing generation is installed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BookmarkTransform {
    /// Row order and row count are unchanged (Replace All, Cleanup, column edits, in-place Save /
    /// Splice-Save); keep logical row indices, bump `source_generation`, and refresh byte offsets.
    #[default]
    PreserveSameRows,
    /// Blank rows were inserted before physical record `before_row`; shift bookmarks at or after
    /// `before_row` downward by `count`, bump `source_generation`, and refresh byte offsets.
    ShiftInsertRows { before_row: u64, count: u64 },
    /// Physical record ranges in `deleted_ranges` were deleted; remove bookmarks inside deleted
    /// ranges, shift surviving bookmarks above them upward, bump `source_generation`, and refresh
    /// byte offsets.
    ShiftDeleteRows {
        deleted_ranges: Vec<RangeInclusive<u64>>,
    },
    /// Row order changed or rows were deduplicated (Sort, Reverse, Shuffle, Delete Duplicates);
    /// clear bookmarks with notice (the prior `WorkingCopySnapshot` preserves them for Undo).
    ClearRowOrderChanged,
}

/// Session-only, bounded collection of row bookmarks for one open document or section.
///
/// Enforces at most one bookmark per logical CSV record and at most [`MAX_ROW_BOOKMARKS`] entries.
/// Never persisted to disk in v1 so file paths and identities are never written outside the session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowBookmarks {
    by_row: BTreeMap<u64, RowBookmark>,
}

impl RowBookmarks {
    pub fn new() -> Self {
        Self {
            by_row: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.by_row.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_row.is_empty()
    }

    pub fn contains_row(&self, local_row: u64) -> bool {
        self.by_row.contains_key(&local_row)
    }

    pub fn get(&self, local_row: u64) -> Option<&RowBookmark> {
        self.by_row.get(&local_row)
    }

    pub fn iter(&self) -> impl Iterator<Item = &RowBookmark> {
        self.by_row.values()
    }

    pub fn all(&self) -> Vec<RowBookmark> {
        self.by_row.values().copied().collect()
    }

    /// Toggles a bookmark on logical CSV record `local_row`.
    ///
    /// Returns `Ok(true)` when a bookmark was added and `Ok(false)` when an existing bookmark was
    /// removed. Enforces one target per logical record (matching either `local_row` or
    /// `(source_generation, record_start_offset)`) and the fixed [`MAX_ROW_BOOKMARKS`] limit.
    pub fn toggle(
        &mut self,
        local_row: u64,
        source_generation: u64,
        record_start_offset: Option<u64>,
        global_ordinal: Option<u64>,
    ) -> Result<bool, String> {
        if self.by_row.remove(&local_row).is_some() {
            return Ok(false);
        }
        if let Some(offset) = record_start_offset
            && let Some(existing_row) = self.by_row.values().find_map(|bookmark| {
                (bookmark.key.source_generation == source_generation
                    && bookmark.key.record_start_offset == Some(offset))
                .then_some(bookmark.local_row)
            })
        {
            self.by_row.remove(&existing_row);
            return Ok(false);
        }
        if self.by_row.len() >= MAX_ROW_BOOKMARKS {
            return Err(format!(
                "Maximum of {MAX_ROW_BOOKMARKS} row bookmarks reached. Clear a bookmark before adding another."
            ));
        }
        self.by_row.insert(
            local_row,
            RowBookmark {
                local_row,
                key: BookmarkKey {
                    source_generation,
                    record_start_offset,
                    global_ordinal,
                },
            },
        );
        Ok(true)
    }

    pub fn remove_row(&mut self, local_row: u64) -> bool {
        self.by_row.remove(&local_row).is_some()
    }

    pub fn clear(&mut self) -> usize {
        let count = self.by_row.len();
        self.by_row.clear();
        count
    }

    /// Returns the next bookmark strictly after `current_row`, wrapping around to the first.
    pub fn next_after(&self, current_row: Option<u64>) -> Option<&RowBookmark> {
        if self.by_row.is_empty() {
            return None;
        }
        if let Some(row) = current_row
            && let Some((_, next)) = self
                .by_row
                .range((Bound::Excluded(row), Bound::Unbounded))
                .next()
        {
            return Some(next);
        }
        self.by_row.values().next()
    }

    /// Returns the previous bookmark strictly before `current_row`, wrapping around to the last.
    pub fn prev_before(&self, current_row: Option<u64>) -> Option<&RowBookmark> {
        if self.by_row.is_empty() {
            return None;
        }
        if let Some(row) = current_row
            && let Some((_, prev)) = self.by_row.range(..row).next_back()
        {
            return Some(prev);
        }
        self.by_row.values().next_back()
    }

    /// Applies a structural or save generation transition in place.
    ///
    /// Returns the number of bookmarks removed or cleared by the transition.
    pub fn apply_transform(&mut self, new_generation: u64, transform: &BookmarkTransform) -> usize {
        match transform {
            BookmarkTransform::ClearRowOrderChanged => self.clear(),
            BookmarkTransform::PreserveSameRows => {
                for bookmark in self.by_row.values_mut() {
                    bookmark.key.source_generation = new_generation;
                    bookmark.key.record_start_offset = None;
                }
                0
            }
            BookmarkTransform::ShiftInsertRows { before_row, count } => {
                let old = std::mem::take(&mut self.by_row);
                for (row, mut bookmark) in old {
                    if row >= *before_row {
                        bookmark.local_row = row.saturating_add(*count);
                        bookmark.key.global_ordinal = bookmark
                            .key
                            .global_ordinal
                            .map(|ordinal| ordinal.saturating_add(*count));
                    }
                    bookmark.key.source_generation = new_generation;
                    bookmark.key.record_start_offset = None;
                    self.by_row.insert(bookmark.local_row, bookmark);
                }
                0
            }
            BookmarkTransform::ShiftDeleteRows { deleted_ranges } => {
                let merged = merge_sorted_ranges(deleted_ranges);
                let old = std::mem::take(&mut self.by_row);
                let mut removed = 0_usize;
                for (row, mut bookmark) in old {
                    let mut deleted_before = 0_u64;
                    let mut deleted = false;
                    for range in &merged {
                        if *range.end() < row {
                            deleted_before =
                                deleted_before.saturating_add(range.end() - range.start() + 1);
                        } else if *range.start() <= row {
                            deleted = true;
                            break;
                        } else {
                            break;
                        }
                    }
                    if deleted {
                        removed += 1;
                        continue;
                    }
                    bookmark.local_row = row.saturating_sub(deleted_before);
                    bookmark.key.global_ordinal = bookmark
                        .key
                        .global_ordinal
                        .map(|ordinal| ordinal.saturating_sub(deleted_before));
                    bookmark.key.source_generation = new_generation;
                    bookmark.key.record_start_offset = None;
                    self.by_row.insert(bookmark.local_row, bookmark);
                }
                removed
            }
        }
    }

    /// Records the resolved `record_start_offset` for `local_row` if it belongs to `source_generation`.
    pub fn set_record_offset(
        &mut self,
        local_row: u64,
        source_generation: u64,
        offset: u64,
    ) -> bool {
        let Some(bookmark) = self.by_row.get_mut(&local_row) else {
            return false;
        };
        if bookmark.key.source_generation != source_generation {
            return false;
        }
        bookmark.key.record_start_offset = Some(offset);
        true
    }

    /// Updates every bookmark's `global_ordinal` (used when `Verify Boundary` upgrades a section in place).
    pub fn backfill_global_ordinals(&mut self, mut resolve: impl FnMut(u64) -> Option<u64>) {
        for bookmark in self.by_row.values_mut() {
            bookmark.key.global_ordinal = resolve(bookmark.local_row);
        }
    }

    /// Returns all bookmarked `local_row`s in `source_generation` that still need a byte offset resolved.
    pub fn pending_offset_rows(&self, source_generation: u64) -> Vec<u64> {
        self.by_row
            .values()
            .filter(|bookmark| {
                bookmark.key.source_generation == source_generation
                    && bookmark.key.record_start_offset.is_none()
            })
            .map(|bookmark| bookmark.local_row)
            .collect()
    }

    /// Snapshots verified split-at-bookmarks targets for Phase 7A-1.
    ///
    /// Drops the first data row (`local_row <= data_start`, avoiding an empty first part),
    /// deduplicates by `record_start_offset`, and requires every retained bookmark to have a
    /// resolved `record_start_offset`.
    pub fn split_targets(&self, data_start: u64) -> Result<Vec<BookmarkSplitTarget>, String> {
        let mut seen_offsets = BTreeSet::new();
        let mut targets = Vec::new();
        for bookmark in self.by_row.values() {
            if bookmark.local_row <= data_start {
                continue;
            }
            let Some(record_start_offset) = bookmark.key.record_start_offset else {
                return Err(
                    "Wait for indexing to finish resolving bookmark offsets before splitting."
                        .into(),
                );
            };
            if seen_offsets.insert(record_start_offset) {
                targets.push(BookmarkSplitTarget {
                    source_generation: bookmark.key.source_generation,
                    local_row: bookmark.local_row,
                    record_start_offset,
                    global_ordinal: bookmark.key.global_ordinal,
                });
            }
        }
        targets.sort_by_key(|target| target.record_start_offset);
        Ok(targets)
    }
}

fn merge_sorted_ranges(ranges: &[RangeInclusive<u64>]) -> Vec<RangeInclusive<u64>> {
    let mut normalized: Vec<RangeInclusive<u64>> = ranges
        .iter()
        .filter(|range| range.start() <= range.end())
        .cloned()
        .collect();
    normalized.sort_by_key(|range| (*range.start(), *range.end()));
    let mut merged: Vec<RangeInclusive<u64>> = Vec::with_capacity(normalized.len());
    for range in normalized {
        if let Some(last) = merged.last_mut()
            && *range.start() <= last.end().saturating_add(1)
        {
            let start = *last.start();
            let end = (*last.end()).max(*range.end());
            *last = start..=end;
        } else {
            merged.push(range);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_enforces_uniqueness_and_fixed_cap() {
        let mut bookmarks = RowBookmarks::new();
        assert!(bookmarks.toggle(5, 1, Some(120), Some(5)).unwrap());
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(
            bookmarks.get(5).unwrap().key,
            BookmarkKey {
                source_generation: 1,
                record_start_offset: Some(120),
                global_ordinal: Some(5),
            }
        );

        // Toggling same local row removes it.
        assert!(!bookmarks.toggle(5, 1, Some(120), Some(5)).unwrap());
        assert!(bookmarks.is_empty());

        // Toggling a colliding (source_generation, record_start_offset) removes the existing target.
        assert!(bookmarks.toggle(5, 1, Some(120), Some(5)).unwrap());
        assert!(!bookmarks.toggle(99, 1, Some(120), Some(99)).unwrap());
        assert!(bookmarks.is_empty());

        // Fixed cap enforcement.
        for row in 1..=(MAX_ROW_BOOKMARKS as u64) {
            assert!(bookmarks.toggle(row, 1, Some(row * 10), Some(row)).unwrap());
        }
        assert_eq!(bookmarks.len(), MAX_ROW_BOOKMARKS);
        let err = bookmarks
            .toggle(MAX_ROW_BOOKMARKS as u64 + 1, 1, Some(999_999), None)
            .unwrap_err();
        assert!(err.contains("10000") || err.contains("10,000"));
    }

    #[test]
    fn navigation_wraps_in_both_directions() {
        let mut bookmarks = RowBookmarks::new();
        bookmarks.toggle(2, 1, Some(20), Some(2)).unwrap();
        bookmarks.toggle(7, 1, Some(70), Some(7)).unwrap();
        bookmarks.toggle(15, 1, Some(150), Some(15)).unwrap();

        assert_eq!(bookmarks.next_after(None).unwrap().local_row, 2);
        assert_eq!(bookmarks.next_after(Some(2)).unwrap().local_row, 7);
        assert_eq!(bookmarks.next_after(Some(7)).unwrap().local_row, 15);
        assert_eq!(bookmarks.next_after(Some(15)).unwrap().local_row, 2);

        assert_eq!(bookmarks.prev_before(None).unwrap().local_row, 15);
        assert_eq!(bookmarks.prev_before(Some(15)).unwrap().local_row, 7);
        assert_eq!(bookmarks.prev_before(Some(7)).unwrap().local_row, 2);
        assert_eq!(bookmarks.prev_before(Some(2)).unwrap().local_row, 15);
    }

    #[test]
    fn insert_and_delete_shifts_are_deterministic() {
        let mut bookmarks = RowBookmarks::new();
        bookmarks.toggle(2, 1, Some(20), Some(2)).unwrap();
        bookmarks.toggle(5, 1, Some(50), Some(5)).unwrap();
        bookmarks.toggle(8, 1, Some(80), Some(8)).unwrap();

        // Insert 3 rows before physical row 5: row 2 stays, rows 5 & 8 shift to 8 & 11.
        let removed = bookmarks.apply_transform(
            2,
            &BookmarkTransform::ShiftInsertRows {
                before_row: 5,
                count: 3,
            },
        );
        assert_eq!(removed, 0);
        let rows: Vec<_> = bookmarks
            .iter()
            .map(|b| {
                (
                    b.local_row,
                    b.key.source_generation,
                    b.key.record_start_offset,
                    b.key.global_ordinal,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (2, 2, None, Some(2)),
                (8, 2, None, Some(8)),
                (11, 2, None, Some(11)),
            ]
        );

        // Delete unsorted/overlapping ranges [7..=9] and [1..=1]:
        // row 2 shifts down by 1 -> 1; row 8 deleted; row 11 shifts down by 1 + 3 = 4 -> 7.
        let removed = bookmarks.apply_transform(
            3,
            &BookmarkTransform::ShiftDeleteRows {
                deleted_ranges: vec![8..=9, 1..=1, 7..=8],
            },
        );
        assert_eq!(removed, 1);
        let rows: Vec<_> = bookmarks
            .iter()
            .map(|b| (b.local_row, b.key.source_generation, b.key.global_ordinal))
            .collect();
        assert_eq!(rows, vec![(1, 3, Some(1)), (7, 3, Some(7))]);
    }

    #[test]
    fn split_targets_drops_first_data_row_and_requires_offsets() {
        let mut bookmarks = RowBookmarks::new();
        // With header (data_start = 1): row 1 is the first data row and must be dropped.
        bookmarks.toggle(1, 1, Some(10), Some(1)).unwrap();
        bookmarks.toggle(4, 1, Some(64), Some(4)).unwrap();
        bookmarks.toggle(9, 1, Some(140), Some(9)).unwrap();

        let targets = bookmarks.split_targets(1).unwrap();
        assert_eq!(
            targets,
            vec![
                BookmarkSplitTarget {
                    source_generation: 1,
                    local_row: 4,
                    record_start_offset: 64,
                    global_ordinal: Some(4),
                },
                BookmarkSplitTarget {
                    source_generation: 1,
                    local_row: 9,
                    record_start_offset: 140,
                    global_ordinal: Some(9),
                },
            ]
        );

        // Unresolved offset must fail cleanly until re-indexed.
        bookmarks.apply_transform(2, &BookmarkTransform::PreserveSameRows);
        assert!(bookmarks.split_targets(1).is_err());
    }
}
