//! Owned private generations retained by documents, jobs, and join captures.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::QuarryError;
use crate::destination::StagedDestination;
use crate::spill::{FileIdentity, SpillWorkspace, remove_owned_named};

/// A retained report channel remains readable after the last storage owner drops.
#[derive(Clone, Default)]
pub struct WorkingCopyCleanup {
    errors: Arc<Mutex<Vec<String>>>,
}

impl WorkingCopyCleanup {
    pub fn take_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.errors.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn report(&self, error: impl std::fmt::Display) {
        let mut errors = self.errors.lock().unwrap_or_else(|e| e.into_inner());
        if errors.len() < 16 {
            errors.push(format!("Could not remove private working files: {error}"));
        } else {
            errors[15] = "Additional private working-file cleanup failures occurred.".into();
        }
    }
}

/// A pinned directory whose cleanup can remove only files published by its jobs.
/// Keep an Arc for every document, background operation, and captured join input.
pub struct WorkingCopyStorage {
    workspace: Option<SpillWorkspace>,
    files: Mutex<BTreeMap<OsString, FileIdentity>>,
    cleanup: WorkingCopyCleanup,
}

impl WorkingCopyStorage {
    pub fn new_in(parent: &Path, cleanup: WorkingCopyCleanup) -> Result<Self, QuarryError> {
        Ok(Self {
            workspace: Some(SpillWorkspace::create_in(parent, "working")?),
            files: Mutex::new(BTreeMap::new()),
            cleanup,
        })
    }

    /// Diagnostic original path. Use current_path before starting another writer.
    pub fn path(&self) -> &Path {
        &self.workspace.as_ref().expect("live working storage").path
    }

    pub fn current_path(&self) -> io::Result<PathBuf> {
        let workspace = self.workspace.as_ref().expect("live working storage");
        workspace.with_directory(|directory| {
            if std::fs::symlink_metadata(&workspace.path).is_ok_and(|m| m.is_dir())
                && directory.still_named_by(&workspace.path)
            {
                Ok(workspace.path.clone())
            } else {
                directory.current_path()
            }
        })
    }

    pub fn contains(&self, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        parent == self.path()
            || self
                .workspace
                .as_ref()
                .expect("live working storage")
                .with_directory(|directory| Ok(directory.still_named_by(parent)))
                .unwrap_or(false)
    }

    /// Delete only a registered generation, through the captured directory.
    /// An unregistered name never gives the caller authority to delete a file.
    pub fn remove(&self, path: &Path) -> io::Result<()> {
        if !self.contains(path) {
            return Err(io::Error::other(
                "The file is outside this working directory.",
            ));
        }
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::other("Missing working filename."))?;
        let mut files = self
            .files
            .lock()
            .map_err(|_| io::Error::other("Working ownership was interrupted."))?;
        let Some(identity) = files.get(name) else {
            return Ok(());
        };
        self.workspace
            .as_ref()
            .expect("live working storage")
            .with_directory(|directory| remove_owned_named(directory, name, identity))?;
        files.remove(name);
        Ok(())
    }

    pub(crate) fn publish(&self, staging: &StagedDestination, file: &File) -> io::Result<()> {
        let mut files = self
            .files
            .lock()
            .map_err(|_| io::Error::other("Working ownership was interrupted."))?;
        self.workspace
            .as_ref()
            .expect("live working storage")
            .with_directory(|directory| {
                if !staging.is_in_directory(directory)? {
                    return Err(io::Error::other(
                        "The private working directory changed before publication.",
                    ));
                }
                let name = staging.file_name().to_owned();
                if files.contains_key(&name) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "A working generation already owns this name.",
                    ));
                }
                let identity = FileIdentity::capture(file)?;
                staging.publish()?;
                files.insert(name, identity);
                Ok(())
            })
    }

    fn cleanup_owned(&mut self) {
        let Some(workspace) = self.workspace.take() else {
            return;
        };
        let files = std::mem::take(self.files.get_mut().unwrap_or_else(|e| e.into_inner()));
        for (name, identity) in files {
            if let Err(error) = workspace
                .with_directory(|directory| remove_owned_named(directory, &name, &identity))
            {
                self.cleanup.report(error);
            }
        }
        if let Err(error) = workspace.cleanup() {
            self.cleanup.report(error);
        }
    }
}

impl Drop for WorkingCopyStorage {
    fn drop(&mut self) {
        self.cleanup_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::destination::PreparedDestination;
    use std::io::Write;

    fn stage(storage: &WorkingCopyStorage, name: &str) -> (StagedDestination, File) {
        let (staging, mut file) =
            PreparedDestination::new(storage.current_path().unwrap().join(name))
                .unwrap()
                .stage()
                .unwrap();
        file.write_all(b"id,value\nx,current\n").unwrap();
        file.sync_all().unwrap();
        (staging, file)
    }

    #[cfg(unix)]
    #[test]
    fn publication_and_cleanup_stay_on_the_captured_directory_after_replacement() {
        let parent = tempfile::tempdir().unwrap();
        let cleanup = WorkingCopyCleanup::default();
        let storage = WorkingCopyStorage::new_in(parent.path(), cleanup.clone()).unwrap();
        let original = storage.path().to_owned();
        let (staging, file) = stage(&storage, "generation-1.csv");
        let moved = parent.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir_all(original.join("foreign")).unwrap();
        std::fs::write(original.join("foreign/keep.csv"), b"preserve").unwrap();
        storage.publish(&staging, &file).unwrap();
        assert_eq!(
            staging.reported_path().unwrap(),
            moved.canonicalize().unwrap().join("generation-1.csv")
        );
        assert_eq!(
            std::fs::read(moved.join("generation-1.csv")).unwrap(),
            b"id,value\nx,current\n"
        );
        drop(file);
        drop(staging);
        drop(storage);
        assert!(!moved.exists());
        assert_eq!(
            std::fs::read(original.join("foreign/keep.csv")).unwrap(),
            b"preserve"
        );
        assert!(cleanup.take_errors().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_writer_cannot_publish_to_a_replacement_of_its_owner_directory() {
        let parent = tempfile::tempdir().unwrap();
        let storage =
            WorkingCopyStorage::new_in(parent.path(), WorkingCopyCleanup::default()).unwrap();
        let original = storage.path().to_owned();
        let moved = parent.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        let (staging, mut file) = PreparedDestination::new(original.join("generation-1.csv"))
            .unwrap()
            .stage()
            .unwrap();
        file.write_all(b"unpublished").unwrap();
        let error = storage.publish(&staging, &file).unwrap_err();
        assert!(error.to_string().contains("changed before publication"));
        assert!(!original.join("generation-1.csv").exists());
        drop(file);
        drop(staging);
        drop(storage);
        assert!(!moved.exists());
        assert_eq!(std::fs::read_dir(original).unwrap().count(), 0);
    }

    #[test]
    fn unregistered_and_replaced_files_are_preserved_and_cleanup_errors_are_retained() {
        let parent = tempfile::tempdir().unwrap();
        let cleanup = WorkingCopyCleanup::default();
        let storage = WorkingCopyStorage::new_in(parent.path(), cleanup.clone()).unwrap();
        let root = storage.path().to_owned();
        let (staging, file) = stage(&storage, "generation-1.csv");
        storage.publish(&staging, &file).unwrap();
        drop(file);
        drop(staging);
        std::fs::rename(
            root.join("generation-1.csv"),
            parent.path().join("original-created.csv"),
        )
        .unwrap();
        std::fs::write(root.join("generation-1.csv"), b"replacement").unwrap();
        std::fs::write(root.join("unregistered.csv"), b"unregistered").unwrap();
        storage.remove(&root.join("unregistered.csv")).unwrap();
        assert!(storage.remove(&root.join("generation-1.csv")).is_err());
        drop(storage);
        assert_eq!(
            std::fs::read(root.join("generation-1.csv")).unwrap(),
            b"replacement"
        );
        assert_eq!(
            std::fs::read(root.join("unregistered.csv")).unwrap(),
            b"unregistered"
        );
        let errors = cleanup.take_errors();
        assert!(
            !errors.is_empty(),
            "cleanup failure must survive the last owner"
        );
        assert!(cleanup.take_errors().is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn a_real_private_job_retains_ownership_and_reports_a_moved_publication() {
        use crate::{
            CleanupOperation, CleanupOutcome, ColumnCleanup, HeaderMode, OpenOptions, Session,
        };
        use std::time::{Duration, Instant};

        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("source.csv");
        let original = b"id,value\nx, current \n";
        std::fs::write(&source, original).unwrap();
        let session = Session::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        let cleanup = WorkingCopyCleanup::default();
        let storage = Arc::new(WorkingCopyStorage::new_in(parent.path(), cleanup.clone()).unwrap());
        let root = storage.path().to_owned();
        let job = session
            .start_create_cleaned_working_copy(
                BTreeMap::new(),
                BTreeMap::new(),
                ColumnCleanup {
                    columns: vec![1],
                    operation: CleanupOperation::Trim,
                },
                root.join("generation-1.csv"),
            )
            .unwrap();
        job.retain_working_storage(Arc::clone(&storage)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !job.progress().done {
            assert!(Instant::now() < deadline, "private handoff stalled");
            std::thread::yield_now();
        }
        let moved = parent.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir_all(root.join("foreign")).unwrap();
        std::fs::write(root.join("foreign/keep.csv"), b"preserve").unwrap();
        let weak = Arc::downgrade(&storage);
        drop(storage);
        let capture = weak
            .upgrade()
            .expect("the pending job must retain its storage owner");
        let CleanupOutcome::Complete(summary) = job.wait().unwrap() else {
            panic!("private publication should complete in its captured directory");
        };
        assert_eq!(
            summary.destination,
            moved.canonicalize().unwrap().join("generation-1.csv")
        );
        assert_eq!(
            std::fs::read(&summary.destination).unwrap(),
            b"id,value\nx,current\n"
        );
        assert_eq!(std::fs::read(&source).unwrap(), original);
        drop(capture);
        assert!(
            !moved.exists(),
            "last owner removes the actual private generation"
        );
        assert_eq!(
            std::fs::read(root.join("foreign/keep.csv")).unwrap(),
            b"preserve"
        );
        assert!(cleanup.take_errors().is_empty());
    }
}
