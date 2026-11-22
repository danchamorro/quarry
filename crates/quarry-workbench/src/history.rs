//! Working-copy storage ownership, structural overlays, and bounded value edit history.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MAX_EDIT_HISTORY_ENTRIES: usize = 1_000;
pub const MAX_EDIT_HISTORY_BYTES: usize = 16 * 1024 * 1024;

// Join captures can retain edit metadata and values without copying them on
// the UI thread. Existing structural/filter snapshots keep their eager map
// copies, so later ordinary edits retain their established allocation behavior.
pub type SharedCellEdits = Arc<BTreeMap<(u64, usize), Arc<Vec<u8>>>>;

pub struct WorkingCopyState {
    pub cleanup: quarry_core::WorkingCopyCleanup,
    // Captured working inputs must own their private generations independently
    // of a tab's lifetime and pruning of obsolete storage roots.
    pub directory: Arc<quarry_core::WorkingCopyStorage>,
    pub previous_directories: Vec<Arc<quarry_core::WorkingCopyStorage>>,
    pub next_generation: u64,
    pub undo: Option<WorkingCopySnapshot>,
    pub redo: Option<WorkingCopySnapshot>,
}

impl WorkingCopyState {
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn new() -> Result<Self, String> {
        Self::new_in(&std::env::temp_dir())
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn new_in(parent: &Path) -> Result<Self, String> {
        Self::new_in_with_cleanup(parent, quarry_core::WorkingCopyCleanup::default())
    }

    pub fn new_in_with_cleanup(
        parent: &Path,
        cleanup: quarry_core::WorkingCopyCleanup,
    ) -> Result<Self, String> {
        Ok(Self {
            cleanup: cleanup.clone(),
            directory: quarry_core::WorkingCopyStorage::new_in(parent, cleanup)
                .map(Arc::new)
                .map_err(|error| {
                    format!(
                        "Cannot create working files in {}: {error}. Choose another folder in File > Temporary storage.",
                        parent.display()
                    )
                })?,
            previous_directories: Vec::new(),
            next_generation: 1,
            undo: None,
            redo: None,
        })
    }

    pub fn next_path(&mut self) -> Result<PathBuf, String> {
        let path = self
            .directory
            .current_path()
            .map_err(|error| format!("Cannot locate private working files: {error}"))?
            .join(format!("generation-{}.csv", self.next_generation));
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or_else(|| "The working generation limit has been reached.".to_owned())?;
        Ok(path)
    }

    pub fn use_directory(&mut self, parent: &Path) -> Result<(), String> {
        let live_parent = self.directory.current_path().ok();
        if live_parent.as_deref().and_then(Path::parent) != Some(parent) {
            let replacement = Self::new_in_with_cleanup(parent, self.cleanup.clone())?;
            self.previous_directories.push(std::mem::replace(
                &mut self.directory,
                replacement.directory,
            ));
        }
        Ok(())
    }

    pub fn prune_directories(&mut self, current: &Path) {
        self.previous_directories.retain(|directory| {
            directory.contains(current)
                || self
                    .undo
                    .iter()
                    .chain(self.redo.iter())
                    .any(|snapshot| directory.contains(&snapshot.path))
        });
    }

    pub fn remove(&self, path: &Path) -> std::io::Result<()> {
        let directory = std::iter::once(&self.directory)
            .chain(self.previous_directories.iter())
            .find(|directory| directory.contains(path))
            .ok_or_else(|| std::io::Error::other("No private working owner for this file."))?;
        directory.remove(path)
    }

    pub fn owns(&self, path: &Path) -> bool {
        self.directory.contains(path)
            || self
                .previous_directories
                .iter()
                .any(|directory| directory.contains(path))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StructuralOverlay {
    pub header_renames: BTreeMap<usize, Arc<String>>,
    pub cell_edits: BTreeMap<(u64, usize), Arc<Vec<u8>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingCopySnapshot {
    pub path: PathBuf,
    pub overlay: StructuralOverlay,
    pub edit_history: EditHistory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditTarget {
    Header(usize),
    Cell(u64, usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueEdit {
    pub target: EditTarget,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
}

impl ValueEdit {
    pub fn bytes(&self) -> usize {
        self.before.as_ref().map_or(0, Vec::len) + self.after.as_ref().map_or(0, Vec::len)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditHistory {
    pub undo: VecDeque<ValueEdit>,
    pub redo: Vec<ValueEdit>,
    pub bytes: usize,
}

impl EditHistory {
    pub fn clear_redo(&mut self) {
        for obsolete in self.redo.drain(..) {
            self.bytes -= obsolete.bytes();
        }
    }

    pub fn record(&mut self, edit: ValueEdit) {
        self.clear_redo();
        let bytes = edit.bytes();
        while self.undo.len() >= MAX_EDIT_HISTORY_ENTRIES
            || self.bytes.saturating_add(bytes) > MAX_EDIT_HISTORY_BYTES
        {
            let Some(obsolete) = self.undo.pop_front() else {
                // An untracked edit is a history boundary, never jump over it.
                return;
            };
            self.bytes -= obsolete.bytes();
        }
        self.bytes += bytes;
        self.undo.push_back(edit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_history_clears_redo_and_enforces_entry_and_byte_bounds() {
        let mut history = EditHistory::default();
        history.record(ValueEdit {
            target: EditTarget::Cell(1, 0),
            before: Some(b"old".to_vec()),
            after: Some(b"new".to_vec()),
        });
        assert_eq!(history.undo.len(), 1);
        assert_eq!(history.bytes, 6);

        let undone = history.undo.pop_back().unwrap();
        history.redo.push(undone);
        history.record(ValueEdit {
            target: EditTarget::Header(0),
            before: None,
            after: Some(b"title".to_vec()),
        });
        assert!(history.redo.is_empty());
        assert_eq!(history.undo.len(), 1);
        assert_eq!(history.bytes, 5);

        for index in 0..MAX_EDIT_HISTORY_ENTRIES + 25 {
            history.record(ValueEdit {
                target: EditTarget::Cell(index as u64, 0),
                before: None,
                after: Some(vec![b'x']),
            });
        }
        assert_eq!(history.undo.len(), MAX_EDIT_HISTORY_ENTRIES);
        assert_eq!(history.bytes, MAX_EDIT_HISTORY_ENTRIES);
    }
}
