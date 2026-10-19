//! Pin a chosen directory before validation, staging, publication and cleanup.
use crate::QuarryError;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Directory {
    path: PathBuf,
    #[cfg(unix)]
    pub(crate) file: File,
    // Denying delete sharing pins every resolved component on Windows, where
    // std path operations otherwise allow a parent to be replaced underneath us.
    #[cfg(windows)]
    _locks: Vec<File>,
}

#[cfg(unix)]
fn directory_flags() -> rustix::fs::OFlags {
    use rustix::fs::OFlags;
    let flags = OFlags::DIRECTORY | OFlags::CLOEXEC;
    #[cfg(target_os = "macos")]
    {
        flags | OFlags::from_bits_retain(libc::O_EVTONLY as _)
    }
    #[cfg(target_os = "linux")]
    {
        flags | OFlags::PATH
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        flags | OFlags::RDONLY
    }
}

#[cfg(unix)]
fn identity(file: &File) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

impl Directory {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let file = File::from(rustix::fs::open(
                path,
                directory_flags(),
                rustix::fs::Mode::empty(),
            )?);
            let resolved = path.canonicalize()?;
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(&resolved)?;
            if identity(&file)? != (metadata.dev(), metadata.ino()) {
                return Err(io::Error::other(
                    "The output directory changed while it was being selected.",
                ));
            }
            Ok(Self {
                path: resolved,
                file,
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            let resolved = path.canonicalize()?;
            let mut prefix = PathBuf::new();
            let mut locks = Vec::new();
            for component in resolved.components() {
                prefix.push(component);
                if matches!(component, std::path::Component::Prefix(_)) {
                    continue;
                }
                let file = fs::OpenOptions::new()
                    // Metadata-only zero-access handles do not enforce delete
                    // sharing on the leaf. Directory read access is required.
                    .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(&prefix)?;
                let metadata = file.metadata()?;
                if !metadata.is_dir()
                    || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                {
                    return Err(io::Error::other(
                        "The resolved output directory changed. Select it again.",
                    ));
                }
                locks.push(file);
                if locks.len() > 1024 {
                    return Err(io::Error::other(
                        "The output directory ancestry is too deep to pin.",
                    ));
                }
            }
            Ok(Self {
                path: resolved,
                _locks: locks,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Pinned output publication is unavailable on this platform.",
            ))
        }
    }

    pub(crate) fn current_path(&self) -> io::Result<PathBuf> {
        self.current_path_using(|| {
            #[cfg(target_os = "macos")]
            let path = {
                use std::os::unix::ffi::OsStringExt;
                PathBuf::from(OsString::from_vec(
                    rustix::fs::getpath(&self.file)?.into_bytes(),
                ))
            };
            #[cfg(target_os = "linux")]
            let path = {
                use std::os::fd::AsRawFd;
                fs::read_link(format!("/proc/self/fd/{}", self.file.as_raw_fd()))?
            };
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            let path = self.path.clone();
            Ok(path)
        })
    }

    fn current_path_using(
        &self,
        locate: impl FnOnce() -> io::Result<PathBuf>,
    ) -> io::Result<PathBuf> {
        // Ordinary cleanup must not require procfs or a platform name locator.
        // The captured leaf itself must remain a directory, not a new symlink.
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.is_dir())
            && self.still_named_by(&self.path)
        {
            return Ok(self.path.clone());
        }
        let path = locate()?;
        if self.still_named_by(&path) {
            Ok(path)
        } else {
            Err(io::Error::other(
                "The captured output directory can no longer be located by name.",
            ))
        }
    }

    pub(crate) fn still_named_by(&self, path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(path)
                .ok()
                .zip(identity(&self.file).ok())
                .is_some_and(|(m, id)| (m.dev(), m.ino()) == id)
        }
        #[cfg(not(unix))]
        {
            path.canonicalize().is_ok_and(|p| p == self.path)
        }
    }
}

/// The destination's directory identity is captured before workspace validation.
pub(crate) struct PreparedDestination {
    requested: PathBuf,
    parent: Arc<Directory>,
    name: OsString,
}
impl PreparedDestination {
    pub(crate) fn new(requested: PathBuf) -> Result<Self, QuarryError> {
        let name = requested
            .file_name()
            .ok_or(QuarryError::InvalidOption(
                "export destination must name a file",
            ))?
            .to_owned();
        let parent_path = crate::storage::output_parent(&requested);
        let parent = Arc::new(
            Directory::open(parent_path)
                .map_err(|error| crate::storage::storage_error(parent_path, error))?,
        );
        Ok(Self {
            requested,
            parent,
            name,
        })
    }

    pub(crate) fn physical_path(&self) -> PathBuf {
        self.parent.path.join(&self.name)
    }

    fn reported_path(&self) -> io::Result<PathBuf> {
        if self
            .parent
            .still_named_by(crate::storage::output_parent(&self.requested))
        {
            Ok(self.requested.clone())
        } else {
            Ok(self.parent.current_path()?.join(&self.name))
        }
    }

    pub(crate) fn ensure_outside_directory(&self, excluded: &Directory) -> Result<(), QuarryError> {
        #[cfg(unix)]
        {
            let excluded = identity(&excluded.file)?;
            let mut current = self.parent.file.try_clone()?;
            for _ in 0..1024 {
                let here = identity(&current)?;
                if here == excluded {
                    return Err(workspace_error());
                }
                let parent = File::from(
                    rustix::fs::openat(
                        &current,
                        "..",
                        directory_flags(),
                        rustix::fs::Mode::empty(),
                    )
                    .map_err(io::Error::from)?,
                );
                if identity(&parent)? == here {
                    return Ok(());
                }
                current = parent;
            }
            Err(io::Error::other("The output directory ancestry is too deep to validate.").into())
        }
        #[cfg(not(unix))]
        {
            if self.parent.path.starts_with(&excluded.path) {
                Err(workspace_error())
            } else {
                Ok(())
            }
        }
    }

    pub(crate) fn stage(self) -> io::Result<(StagedDestination, File)> {
        #[cfg(unix)]
        {
            let create = UnixStage::create(Arc::clone(&self.parent));
            let (stage, file) = match create {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    use std::os::unix::fs::MetadataExt;
                    let root = Arc::new(Directory::open(&std::env::temp_dir())?);
                    if root.file.metadata()?.dev() != self.parent.file.metadata()?.dev() {
                        return Err(error);
                    }
                    UnixStage::create(root)?
                }
                Err(error) => return Err(error),
            };
            let temporary = stage.root.path.join(&stage.name).join("data.tmp");
            Ok((
                StagedDestination {
                    temporary,
                    unix: stage,
                    destination: self,
                },
                file,
            ))
        }
        #[cfg(not(unix))]
        {
            // Cleanup must never recurse into an unrelated entry, including
            // after our own Drop has reported a nonempty-directory error.
            let directory = crate::export::private_staging_directory(&self.parent.path)?.keep();
            let temporary = directory.join("data.tmp");
            let file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => file,
                Err(error) => {
                    let _ = fs::remove_dir(&directory);
                    return Err(error);
                }
            };
            Ok((
                StagedDestination {
                    temporary,
                    directory,
                    destination: self,
                },
                file,
            ))
        }
    }
}

fn workspace_error() -> QuarryError {
    QuarryError::JoinFile {
        path: None,
        record: None,
        reason: "Choose an output folder outside the join's private temporary workspace.".into(),
    }
}

#[cfg(unix)]
struct UnixStage {
    root: Arc<Directory>,
    name: OsString,
    directory: File,
}
#[cfg(unix)]
impl UnixStage {
    fn create(root: Arc<Directory>) -> io::Result<(Self, File)> {
        use rustix::fs::{Mode, OFlags};
        for _ in 0..100 {
            let id = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
            let name = OsString::from(format!(".quarry-export-{}-pin-{id}", std::process::id()));
            match rustix::fs::mkdirat(&root.file, &name, Mode::RWXU) {
                Ok(()) => {}
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => return Err(error.into()),
            }
            // Search/event-only handles permit file-only drop folders on macOS.
            let directory = match rustix::fs::openat(
                &root.file,
                &name,
                directory_flags() | OFlags::NOFOLLOW,
                Mode::empty(),
            ) {
                Ok(fd) => File::from(fd),
                Err(error) => return Err(error.into()),
            };
            let stage = Self {
                root,
                name,
                directory,
            };
            // The new directory must not inherit access grants before its first file.
            #[cfg(target_os = "macos")]
            crate::storage::set_macos_acl(&stage.directory, None)?;
            #[cfg(not(target_os = "linux"))]
            rustix::fs::fchmod(&stage.directory, Mode::RWXU)?;
            // Linux O_PATH cannot be fchmod'ed; mode 0700 is set by mkdirat.
            let file = File::from(rustix::fs::openat(
                &stage.directory,
                "data.tmp",
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::RUSR | Mode::WUSR,
            )?);
            #[cfg(target_os = "macos")]
            crate::storage::set_macos_acl(&file, None)?;
            return Ok((stage, file));
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Could not reserve private output staging.",
        ))
    }

    fn cleanup(&self) -> io::Result<()> {
        use rustix::fs::{AtFlags, Mode, OFlags, unlinkat};
        match unlinkat(&self.directory, "data.tmp", AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        let named = match rustix::fs::openat(
            &self.root.file,
            &self.name,
            directory_flags() | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if identity(&named)? != identity(&self.directory)? {
            return Err(io::Error::other(
                "The private staging directory name changed; the unrelated directory was left untouched.",
            ));
        }
        match unlinkat(&self.root.file, &self.name, AtFlags::REMOVEDIR) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
#[cfg(unix)]
impl Drop for UnixStage {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub(crate) struct StagedDestination {
    temporary: PathBuf,
    #[cfg(unix)]
    unix: UnixStage,
    #[cfg(not(unix))]
    directory: PathBuf,
    // Kept after the staging owner so Windows locks outlive directory cleanup.
    destination: PreparedDestination,
}
impl StagedDestination {
    pub(crate) fn temporary(&self) -> &Path {
        &self.temporary
    }
    pub(crate) fn reported_path(&self) -> io::Result<PathBuf> {
        self.destination.reported_path()
    }

    pub(crate) fn publish(&self) -> io::Result<()> {
        // Fail before publication on platforms that cannot locate a moved parent.
        self.destination.reported_path()?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let renamed = rustix::fs::renameat_with(
                &self.unix.directory,
                "data.tmp",
                &self.destination.parent.file,
                &self.destination.name,
                rustix::fs::RenameFlags::NOREPLACE,
            );
            #[cfg(target_os = "linux")]
            return self.finish_linux_rename(renamed);
            #[cfg(not(target_os = "linux"))]
            renamed.map_err(Into::into)
        }
        #[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
        {
            rustix::fs::linkat(
                &self.unix.directory,
                "data.tmp",
                &self.destination.parent.file,
                &self.destination.name,
                rustix::fs::AtFlags::empty(),
            )
            .map_err(Into::into)
        }
        #[cfg(not(unix))]
        {
            crate::export::publish_no_replace(&self.temporary, &self.destination.physical_path())
        }
    }

    #[cfg(any(target_os = "linux", all(test, unix)))]
    fn finish_linux_rename(&self, renamed: rustix::io::Result<()>) -> io::Result<()> {
        if matches!(renamed, Err(rustix::io::Errno::INVAL)) {
            return rustix::fs::linkat(
                &self.unix.directory,
                "data.tmp",
                &self.destination.parent.file,
                &self.destination.name,
                rustix::fs::AtFlags::empty(),
            )
            .map_err(Into::into);
        }
        renamed.map_err(Into::into)
    }

    pub(crate) fn cleanup(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.unix.cleanup()
        }
        #[cfg(not(unix))]
        {
            match fs::remove_file(&self.temporary) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            match fs::remove_dir(&self.directory) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        }
    }
}
impl Drop for StagedDestination {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn pinned_windows_path_cannot_change_through_parent_rename() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("outer");
        let inside = parent.join("inside");
        fs::create_dir_all(&inside).unwrap();
        let moved = dir.path().join("moved");
        // Establish that ordinary permissions allow this rename.
        fs::rename(&parent, &moved).unwrap();
        fs::rename(&moved, &parent).unwrap();
        let prepared = PreparedDestination::new(inside.join("result.csv")).unwrap();
        assert!(
            fs::rename(&parent, &moved).is_err(),
            "every resolved ancestor must remain pinned"
        );
        assert!(
            fs::rename(&inside, parent.join("other")).is_err(),
            "the selected directory must remain pinned"
        );
        drop(prepared);
        fs::rename(&parent, &moved).unwrap();
    }

    #[test]
    fn pinned_windows_no_replace_keeps_a_racing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.csv");
        let prepared = PreparedDestination::new(path.clone()).unwrap();
        let (staged, mut file) = prepared.stage().unwrap();
        use std::io::Write;
        file.write_all(b"new").unwrap();
        drop(file);
        fs::write(&path, b"existing").unwrap();
        assert_eq!(
            staged.publish().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        staged.cleanup().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"existing");
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn unsupported_linux_rename_uses_pinned_no_clobber_links() {
        for existing in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let destination = dir.path().join("result.csv");
            let (stage, mut file) = PreparedDestination::new(destination.clone())
                .unwrap()
                .stage()
                .unwrap();
            file.write_all(b"new output").unwrap();
            drop(file);
            if existing {
                fs::write(&destination, b"existing").unwrap();
            }
            let result = stage.finish_linux_rename(Err(rustix::io::Errno::INVAL));
            if existing {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            } else {
                result.unwrap();
            }
            stage.cleanup().unwrap();
            assert_eq!(
                fs::read(&destination).unwrap(),
                if existing {
                    b"existing".as_slice()
                } else {
                    b"new output".as_slice()
                }
            );
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("denied.csv");
        let (stage, file) = PreparedDestination::new(destination.clone())
            .unwrap()
            .stage()
            .unwrap();
        drop(file);
        assert_eq!(
            stage
                .finish_linux_rename(Err(rustix::io::Errno::ACCESS))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        stage.cleanup().unwrap();
        assert!(
            !destination.exists(),
            "unrelated rename errors must not trigger a fallback"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn retained_directory_uses_existing_name_without_a_platform_locator() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("owned");
        fs::create_dir(&path).unwrap();
        let directory = Directory::open(&path).unwrap();
        let resolved = directory
            .current_path_using(|| {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "platform locator unavailable",
                ))
            })
            .unwrap();
        assert_eq!(resolved, path.canonicalize().unwrap());
    }

    #[test]
    fn replaced_directory_name_requires_a_matching_platform_location() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("owned");
        fs::create_dir(&path).unwrap();
        let directory = Directory::open(&path).unwrap();
        let moved = parent.path().join("moved");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(
            directory
                .current_path_using(|| {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "platform locator unavailable",
                    ))
                })
                .is_err()
        );
        assert!(directory.current_path_using(|| Ok(path.clone())).is_err());
        assert_eq!(
            directory.current_path_using(|| Ok(moved.clone())).unwrap(),
            moved
        );
        assert!(path.is_dir(), "the unrelated replacement is preserved");
    }
    #[test]
    fn a_symlink_replacing_the_canonical_leaf_uses_the_platform_location() {
        use std::os::unix::fs::symlink;
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("owned");
        fs::create_dir(&path).unwrap();
        let directory = Directory::open(&path).unwrap();
        let moved = parent.path().join("moved");
        fs::rename(&path, &moved).unwrap();
        symlink(&moved, &path).unwrap();
        assert_eq!(
            directory.current_path_using(|| Ok(moved.clone())).unwrap(),
            moved
        );
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }
}
