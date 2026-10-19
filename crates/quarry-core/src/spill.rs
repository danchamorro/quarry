//! Private disk workspaces and framed streams shared by external operations.
//!
//! This module owns storage and framing only. Comparators, resource budgets,
//! cancellation, progress and verification remain with each operation.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::QuarryError;
use crate::destination::{Directory, PreparedDestination};
use crate::storage::storage_error;

static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) struct SpillWorkspace {
    pub(crate) path: PathBuf,
    root: Arc<WorkspaceRoot>,
    next_run: AtomicU64,
    active: bool,
}

struct WorkspaceRoot {
    state: Mutex<WorkspaceState>,
}

struct WorkspaceState {
    directory: Option<Directory>,
    // Only live owned files are retained. Unlinking a run removes its entry.
    files: BTreeMap<u64, FileIdentity>,
}

#[derive(Clone)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    stamp: crate::SourceStamp,
}

impl FileIdentity {
    fn capture(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(changed_file());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                stamp: crate::SourceStamp::from_file(file)?,
            })
        }
    }

    fn matches(&self, file: &File) -> io::Result<bool> {
        let observed = Self::capture(file)?;
        #[cfg(unix)]
        {
            Ok((self.device, self.inode) == (observed.device, observed.inode))
        }
        #[cfg(not(unix))]
        {
            Ok(self.stamp.same_identity(&observed.stamp))
        }
    }
}

/// A spill reference carries its directory identity, not authority to reopen a
/// mutable pathname. The displayed path is only for diagnostics and tests.
#[derive(Clone)]
pub(crate) struct SpillFile {
    path: PathBuf,
    id: u64,
    root: Arc<WorkspaceRoot>,
}

impl std::fmt::Debug for SpillFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.fmt(formatter)
    }
}

impl AsRef<Path> for SpillFile {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for SpillFile {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

fn changed_file() -> io::Error {
    io::Error::other("A private spill file changed or its workspace was released.")
}

fn run_name(id: u64) -> String {
    format!("run-{id}.bin")
}

impl WorkspaceRoot {
    fn lock(&self) -> io::Result<MutexGuard<'_, WorkspaceState>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("Private spill ownership was interrupted."))
    }

    /// Delete only registered regular files through the held directory. Never
    /// recurse into a directory found at the original (replaceable) pathname.
    fn cleanup(&self) -> io::Result<()> {
        let mut state = self.lock()?;
        let Some(directory) = state.directory.take() else {
            return Ok(());
        };
        let files = std::mem::take(&mut state.files);
        let mut failure = None;
        for (id, identity) in files {
            if let Err(error) = remove_owned(&directory, id, &identity) {
                failure.get_or_insert(error);
            }
        }
        if let Err(error) = remove_empty_directory(directory) {
            failure.get_or_insert(error);
        }
        failure.map_or(Ok(()), Err)
    }
}

impl SpillFile {
    pub(crate) fn open(&self) -> io::Result<File> {
        let state = self.root.lock()?;
        let directory = state.directory.as_ref().ok_or_else(changed_file)?;
        let identity = state.files.get(&self.id).ok_or_else(changed_file)?;
        open_owned(directory, self.id, identity)
    }

    pub(crate) fn metadata(&self) -> io::Result<fs::Metadata> {
        self.open()?.metadata()
    }

    pub(crate) fn remove(&self) -> io::Result<()> {
        let mut state = self.root.lock()?;
        let directory = state.directory.as_ref().ok_or_else(changed_file)?;
        let identity = state.files.get(&self.id).ok_or_else(changed_file)?;
        remove_owned(directory, self.id, identity)?;
        state.files.remove(&self.id);
        Ok(())
    }
}

fn open_owned(directory: &Directory, id: u64, identity: &FileIdentity) -> io::Result<File> {
    #[cfg(unix)]
    let file = File::from(rustix::fs::openat(
        &directory.file,
        run_name(id),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )?);
    #[cfg(not(unix))]
    let file = {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        options.open(directory.current_path()?.join(run_name(id)))?
    };
    if !identity.matches(&file)? {
        return Err(changed_file());
    }
    Ok(file)
}

#[allow(clippy::unnecessary_cast)]
fn remove_owned(directory: &Directory, id: u64, identity: &FileIdentity) -> io::Result<()> {
    #[cfg(unix)]
    {
        let name = run_name(id);
        let stat = match rustix::fs::statat(
            &directory.file,
            &name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
            || (stat.st_dev as u64, stat.st_ino as u64) != (identity.device, identity.inode)
        {
            return Err(changed_file());
        }
        rustix::fs::unlinkat(&directory.file, name, rustix::fs::AtFlags::empty())?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        match open_owned(directory, id, identity) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        fs::remove_file(directory.current_path()?.join(run_name(id)))
    }
}

/// rmdir is deliberately nonrecursive even after an identity check. A later
/// name substitution can never turn this into deletion of another tree's data.
fn remove_empty_directory(directory: Directory) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = match directory.file.metadata() {
            Ok(metadata) => metadata,
            // macOS O_EVTONLY handles report ENOENT after rmdir.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.nlink() == 0 {
            return Ok(());
        }
        let current = match directory.current_path() {
            Ok(path) => path,
            // Darwin cannot recover a pathname for a directory already unlinked.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let parent = Directory::open(current.parent().ok_or_else(changed_file)?)?;
        let name = current.file_name().ok_or_else(changed_file)?;
        let observed = File::from(rustix::fs::openat(
            &parent.file,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )?);
        let observed = observed.metadata()?;
        if (metadata.dev(), metadata.ino()) != (observed.dev(), observed.ino()) {
            return Err(changed_file());
        }
        rustix::fs::unlinkat(&parent.file, name, rustix::fs::AtFlags::REMOVEDIR)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let path = directory.current_path()?;
        // Keep ancestors pinned while releasing the leaf's Windows delete lock.
        let _parent = Directory::open(path.parent().ok_or_else(changed_file)?)?;
        drop(directory);
        fs::remove_dir(path)
    }
}

impl SpillWorkspace {
    /// Reserve owner-only storage before writing any source values.
    pub(crate) fn create_in(parent: &Path, namespace: &str) -> Result<Self, QuarryError> {
        if namespace.is_empty()
            || !namespace
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "invalid spill namespace").into(),
            );
        }
        for _ in 0..100 {
            let id = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".quarry-{namespace}-{}-{id}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => {
                    let directory =
                        Directory::open(&path).map_err(|error| storage_error(parent, error))?;
                    if !fs::symlink_metadata(&path)?.is_dir() || !directory.still_named_by(&path) {
                        return Err(changed_file().into());
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        let metadata = directory.file.metadata()?;
                        // SAFETY: geteuid has no arguments or memory preconditions.
                        if metadata.mode() & 0o077 != 0
                            || metadata.uid() != unsafe { libc::geteuid() }
                        {
                            return Err(changed_file().into());
                        }
                    }
                    #[cfg(target_os = "macos")]
                    if let Err(error) = crate::storage::set_macos_acl(&directory.file, None) {
                        let _ = remove_empty_directory(directory);
                        return Err(storage_error(parent, error));
                    }
                    return Ok(Self {
                        path,
                        root: Arc::new(WorkspaceRoot {
                            state: Mutex::new(WorkspaceState {
                                directory: Some(directory),
                                files: BTreeMap::new(),
                            }),
                        }),
                        next_run: AtomicU64::new(0),
                        active: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(storage_error(parent, error)),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("could not reserve a temporary {namespace} directory"),
        )
        .into())
    }

    /// Creates an owner-only spill file relative to the captured directory.
    pub(crate) fn create_run(&self) -> Result<(SpillFile, BufWriter<File>), QuarryError> {
        let id = self.next_run.fetch_add(1, Ordering::Relaxed);
        let mut state = self.root.lock()?;
        let directory = state.directory.as_ref().ok_or_else(changed_file)?;
        #[cfg(unix)]
        let file = File::from(
            rustix::fs::openat(
                &directory.file,
                run_name(id),
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .map_err(|error| storage_error(&self.path, error.into()))?,
        );
        #[cfg(not(unix))]
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.current_path()?.join(run_name(id)))
            .map_err(|error| storage_error(&self.path, error))?;
        let identity = FileIdentity::capture(&file)?;
        #[cfg(target_os = "macos")]
        if let Err(error) = crate::storage::set_macos_acl(&file, None) {
            drop(file);
            let _ = remove_owned(directory, id, &identity);
            return Err(storage_error(&self.path, error));
        }
        state.files.insert(id, identity);
        Ok((
            SpillFile {
                path: self.path.join(run_name(id)),
                id,
                root: Arc::clone(&self.root),
            },
            BufWriter::new(file),
        ))
    }

    pub(crate) fn ensure_output_outside(
        &self,
        destination: &PreparedDestination,
    ) -> Result<(), QuarryError> {
        let state = self.root.lock()?;
        destination.ensure_outside_directory(state.directory.as_ref().ok_or_else(changed_file)?)
    }

    /// Remove owned scratch data explicitly so cleanup failures reach the caller.
    pub(crate) fn cleanup(mut self) -> Result<(), QuarryError> {
        self.active = false;
        self.root.cleanup().map_err(Into::into)
    }
}

impl Drop for SpillWorkspace {
    /// Best-effort fallback uses the same nonrecursive, identity-bound cleanup.
    fn drop(&mut self) {
        if self.active {
            let _ = self.root.cleanup();
        }
    }
}

/// A frame begins with three little-endian u64s: key length, record length,
/// and source ordinal. The key follows; the record remains unread until needed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FrameHead {
    pub(crate) key: Vec<u8>,
    pub(crate) record_len: usize,
    pub(crate) ordinal: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct FrameLimits {
    pub(crate) max_key_bytes: usize,
    pub(crate) max_record_bytes: usize,
}

#[derive(Debug)]
pub(crate) enum FrameReadError {
    Io(io::Error),
    KeyLength,
    RecordLength,
}

impl From<io::Error> for FrameReadError {
    /// Preserve the underlying I/O failure separately from invalid frame lengths.
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Keep the existing sort frame byte-for-byte compatible. Bounds on accepted
/// source values belong to the producer; readers independently validate lengths.
pub(crate) fn write_frame(
    writer: &mut impl Write,
    key: &[u8],
    record: &[u8],
    ordinal: u64,
) -> io::Result<u64> {
    let key_len = u64::try_from(key.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "spill key length overflow"))?;
    let record_len = u64::try_from(record.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "spill record length overflow"))?;
    let frame_len = 24_u64
        .checked_add(key_len)
        .and_then(|n| n.checked_add(record_len))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "spill frame length overflow")
        })?;
    writer.write_all(&key_len.to_le_bytes())?;
    writer.write_all(&record_len.to_le_bytes())?;
    writer.write_all(&ordinal.to_le_bytes())?;
    writer.write_all(key)?;
    writer.write_all(record)?;
    Ok(frame_len)
}

/// Reject both declared lengths before allocating the key. The caller must
/// consume this head's complete record before requesting the next frame.
pub(crate) fn read_head(
    reader: &mut impl Read,
    limits: FrameLimits,
) -> Result<Option<FrameHead>, FrameReadError> {
    let mut header = [0_u8; 24];
    if reader.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..])?;
    let key_len = u64::from_le_bytes(header[0..8].try_into().unwrap());
    let record_len = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let ordinal = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let key_len = usize::try_from(key_len)
        .ok()
        .filter(|length| *length <= limits.max_key_bytes)
        .ok_or(FrameReadError::KeyLength)?;
    let record_len = usize::try_from(record_len)
        .ok()
        .filter(|length| *length <= limits.max_record_bytes)
        .ok_or(FrameReadError::RecordLength)?;
    let mut key = vec![0_u8; key_len];
    reader.read_exact(&mut key)?;
    Ok(Some(FrameHead {
        key,
        record_len,
        ordinal,
    }))
}

/// Validate the body bound even when called without a previously read head.
pub(crate) fn read_record(
    reader: &mut impl Read,
    length: usize,
    max_record_bytes: usize,
) -> Result<Vec<u8>, FrameReadError> {
    if length > max_record_bytes {
        return Err(FrameReadError::RecordLength);
    }
    let mut record = vec![0_u8; length];
    reader.read_exact(&mut record)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Tiny bounds exercise oversized declarations without allocating large payloads.
    fn limits() -> FrameLimits {
        FrameLimits {
            max_key_bytes: 3,
            max_record_bytes: 4,
        }
    }

    #[test]
    fn frames_keep_the_existing_wire_format_and_stream_one_body_at_a_time() {
        let mut bytes = Vec::new();
        assert_eq!(
            write_frame(&mut bytes, b"a\0z", b"x,\ny", u64::MAX).unwrap(),
            31
        );
        let mut expected = vec![3, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0];
        expected.extend_from_slice(&[255; 8]);
        expected.extend_from_slice(b"a\0zx,\ny");
        assert_eq!(bytes, expected);
        write_frame(&mut bytes, b"", b"", 0).unwrap();
        let mut reader = Cursor::new(bytes);
        let head = read_head(&mut reader, limits()).unwrap().unwrap();
        assert_eq!(
            head,
            FrameHead {
                key: b"a\0z".to_vec(),
                record_len: 4,
                ordinal: u64::MAX
            }
        );
        assert_eq!(reader.position(), 27);
        assert_eq!(
            read_record(&mut reader, head.record_len, 4).unwrap(),
            b"x,\ny"
        );
        let empty = read_head(&mut reader, limits()).unwrap().unwrap();
        assert_eq!(
            empty,
            FrameHead {
                key: vec![],
                record_len: 0,
                ordinal: 0
            }
        );
        assert!(read_record(&mut reader, 0, 0).unwrap().is_empty());
        assert!(read_head(&mut reader, limits()).unwrap().is_none());
    }

    #[test]
    fn truncated_frames_never_look_like_clean_eof() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, b"abc", b"data", 42).unwrap();
        for length in 1..bytes.len() {
            let mut reader = Cursor::new(&bytes[..length]);
            let result = read_head(&mut reader, limits()).and_then(|head| {
                let head = head.expect("partial frame is not EOF");
                read_record(&mut reader, head.record_len, 4)
            });
            assert!(
                matches!(result, Err(FrameReadError::Io(ref error))
                if error.kind() == io::ErrorKind::UnexpectedEof),
                "length {length}"
            );
        }
    }

    #[test]
    fn oversized_lengths_are_rejected_before_payload_reads() {
        for (key, record, key_error) in [
            (4, 0, true),
            (u64::MAX, 0, true),
            (3, 5, false),
            (0, u64::MAX, false),
        ] {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&key.to_le_bytes());
            bytes.extend_from_slice(&record.to_le_bytes());
            bytes.extend_from_slice(&0_u64.to_le_bytes());
            let mut reader = Cursor::new(bytes);
            let error = read_head(&mut reader, limits()).unwrap_err();
            assert!(matches!(
                (&error, key_error),
                (FrameReadError::KeyLength, true) | (FrameReadError::RecordLength, false)
            ));
            assert_eq!(reader.position(), 24);
        }
        let mut reader = Cursor::new(b"unread");
        assert!(matches!(
            read_record(&mut reader, usize::MAX, 4),
            Err(FrameReadError::RecordLength)
        ));
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn workspaces_preserve_names_are_unique_and_clean_up_only_their_files() {
        let parent = tempfile::tempdir().unwrap();
        let unrelated = parent.path().join("keep.csv");
        fs::write(&unrelated, b"untouched").unwrap();
        let one = SpillWorkspace::create_in(parent.path(), "sort").unwrap();
        let two = SpillWorkspace::create_in(parent.path(), "join").unwrap();
        assert_ne!(one.path, two.path);
        assert!(
            one.path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(&format!(".quarry-sort-{}-", std::process::id()))
        );
        let first_path = one.path.clone();
        let second_path = two.path.clone();
        let (run, mut writer) = one.create_run().unwrap();
        assert_eq!(run.file_name().unwrap(), "run-0.bin");
        writer.write_all(b"private").unwrap();
        writer.flush().unwrap();
        let (next, _) = one.create_run().unwrap();
        assert_eq!(next.file_name().unwrap(), "run-1.bin");
        drop(writer);
        one.cleanup().unwrap();
        assert!(!first_path.exists());
        assert!(second_path.exists());
        drop(two);
        assert!(!second_path.exists());
        assert_eq!(fs::read(unrelated).unwrap(), b"untouched");
    }

    #[cfg(unix)]
    #[test]
    fn renamed_workspaces_keep_reads_writes_and_unlinks_on_the_captured_directory() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = SpillWorkspace::create_in(parent.path(), "join").unwrap();
        let original = workspace.path.clone();
        let moved = parent.path().join("moved");
        let (old_run, mut writer) = workspace.create_run().unwrap();
        writer.write_all(b"owned").unwrap();
        writer.flush().unwrap();
        drop(writer);
        fs::rename(&original, &moved).unwrap();
        fs::create_dir(&original).unwrap();
        fs::write(original.join("run-0.bin"), b"unrelated").unwrap();
        let mut bytes = Vec::new();
        old_run.open().unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"owned");
        let (new_run, mut writer) = workspace.create_run().unwrap();
        writer.write_all(b"new owned data").unwrap();
        writer.flush().unwrap();
        drop(writer);
        assert_eq!(
            fs::read(moved.join("run-1.bin")).unwrap(),
            b"new owned data"
        );
        assert!(!original.join("run-1.bin").exists());
        old_run.remove().unwrap();
        assert!(!moved.join("run-0.bin").exists());
        let selected = PreparedDestination::new(moved.join("result.csv")).unwrap();
        assert!(workspace.ensure_output_outside(&selected).is_err());
        drop(selected);
        workspace.cleanup().unwrap();
        assert!(!moved.exists());
        assert_eq!(fs::read(original.join("run-0.bin")).unwrap(), b"unrelated");
        assert!(
            new_run.open().is_err(),
            "released references cannot reopen a substitute"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_retained_workspace_pins_its_name_and_ancestors_until_cleanup() {
        let parent = tempfile::tempdir().unwrap();
        let container = parent.path().join("container");
        fs::create_dir(&container).unwrap();
        let workspace = SpillWorkspace::create_in(&container, "join").unwrap();
        let (run, writer) = workspace.create_run().unwrap();
        drop(writer);
        assert!(fs::rename(&workspace.path, container.join("moved")).is_err());
        assert!(fs::rename(&container, parent.path().join("moved-container")).is_err());
        workspace.cleanup().unwrap();
        assert!(run.open().is_err());
        let moved = parent.path().join("moved-container");
        fs::rename(&container, &moved).unwrap();
        fs::rename(&moved, &container).unwrap();
        assert_eq!(fs::read_dir(&container).unwrap().count(), 0);
    }

    #[test]
    fn cleanup_and_drop_preserve_unregistered_nested_data_and_report_failure() {
        for explicit in [true, false] {
            let parent = tempfile::tempdir().unwrap();
            let workspace = SpillWorkspace::create_in(parent.path(), "join").unwrap();
            let path = workspace.path.clone();
            let (run, writer) = workspace.create_run().unwrap();
            drop(writer);
            fs::create_dir(path.join("unrelated")).unwrap();
            fs::write(path.join("unrelated/keep.csv"), b"untouched").unwrap();
            if explicit {
                assert!(
                    workspace.cleanup().is_err(),
                    "nonempty workspace must report failed cleanup"
                );
            } else {
                drop(workspace);
            }
            assert_eq!(
                fs::read(path.join("unrelated/keep.csv")).unwrap(),
                b"untouched"
            );
            assert!(!run.exists(), "the registered spill is removed");
        }
    }

    #[test]
    fn substituted_run_files_are_not_read_or_deleted() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = SpillWorkspace::create_in(parent.path(), "join").unwrap();
        let (run, writer) = workspace.create_run().unwrap();
        drop(writer);
        // Retain the original inode, so replacement cannot reuse its identity.
        fs::rename(run.as_ref(), parent.path().join("original-owned")).unwrap();
        fs::write(run.as_ref(), b"unrelated").unwrap();
        assert!(run.open().is_err());
        assert!(run.remove().is_err());
        assert!(workspace.cleanup().is_err());
        assert_eq!(fs::read(run.as_ref()).unwrap(), b"unrelated");
    }

    #[cfg(unix)]
    #[test]
    fn substituted_symlinks_and_directories_are_never_followed_or_recursed() {
        use std::os::unix::fs::symlink;
        for directory in [false, true] {
            let parent = tempfile::tempdir().unwrap();
            let workspace = SpillWorkspace::create_in(parent.path(), "join").unwrap();
            let (run, writer) = workspace.create_run().unwrap();
            drop(writer);
            let outside = parent.path().join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("keep.csv"), b"untouched").unwrap();
            fs::remove_file(run.as_ref()).unwrap();
            if directory {
                fs::create_dir(run.as_ref()).unwrap();
                fs::write(run.join("keep.csv"), b"nested").unwrap();
            } else {
                symlink(&outside, run.as_ref()).unwrap();
            }
            assert!(run.open().is_err());
            assert!(run.remove().is_err());
            assert!(workspace.cleanup().is_err());
            assert_eq!(fs::read(outside.join("keep.csv")).unwrap(), b"untouched");
            assert_eq!(
                fs::read(run.join("keep.csv")).unwrap(),
                if directory {
                    b"nested".as_slice()
                } else {
                    b"untouched".as_slice()
                }
            );
        }
    }

    #[test]
    fn unlinking_runs_does_not_retain_metadata_for_previous_passes() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = SpillWorkspace::create_in(parent.path(), "join").unwrap();
        for _ in 0..256 {
            let (run, writer) = workspace.create_run().unwrap();
            drop(writer);
            assert_eq!(workspace.root.lock().unwrap().files.len(), 1);
            run.remove().unwrap();
            assert!(workspace.root.lock().unwrap().files.is_empty());
        }
        workspace.cleanup().unwrap();
    }

    #[test]
    fn missing_workspaces_clean_up_and_invalid_names_create_nothing() {
        let parent = tempfile::tempdir().unwrap();
        for namespace in ["", "../escape", "/absolute", "a/b", "a\\b"] {
            assert!(SpillWorkspace::create_in(parent.path(), namespace).is_err());
        }
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
        let workspace = SpillWorkspace::create_in(parent.path(), "join").unwrap();
        #[cfg(unix)]
        fs::remove_dir(&workspace.path).unwrap();
        #[cfg(windows)]
        assert!(
            fs::remove_dir(&workspace.path).is_err(),
            "the retained directory is pinned"
        );
        workspace.cleanup().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn every_namespace_uses_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let workspace = SpillWorkspace::create_in(parent.path(), "join").unwrap();
        assert_eq!(
            fs::metadata(&workspace.path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let (run, writer) = workspace.create_run().unwrap();
        assert_eq!(
            fs::metadata(run).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(writer);
        workspace.cleanup().unwrap();
    }
}
