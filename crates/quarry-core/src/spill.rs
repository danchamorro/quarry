//! Private disk workspaces and framed streams shared by external operations.
//!
//! This module owns storage and framing only. Comparators, resource budgets,
//! cancellation, progress and verification remain with each operation.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::QuarryError;
use crate::storage::storage_error;

static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) struct SpillWorkspace {
    pub(crate) path: PathBuf,
    next_run: AtomicU64,
    active: bool,
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
                    #[cfg(target_os = "macos")]
                    if let Err(error) = File::open(&path)
                        .and_then(|directory| crate::storage::set_macos_acl(&directory, None))
                    {
                        let _ = fs::remove_dir(&path);
                        return Err(storage_error(parent, error));
                    }
                    return Ok(Self {
                        path,
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

    /// Creates an owner-only spill file without inherited macOS access grants.
    pub(crate) fn create_run(&self) -> Result<(PathBuf, BufWriter<File>), QuarryError> {
        let id = self.next_run.fetch_add(1, Ordering::Relaxed);
        let path = self.path.join(format!("run-{id}.bin"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&path)
            .map_err(|error| storage_error(&self.path, error))?;
        #[cfg(target_os = "macos")]
        if let Err(error) = crate::storage::set_macos_acl(&file, None) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(storage_error(&self.path, error));
        }
        Ok((path, BufWriter::new(file)))
    }

    pub(crate) fn cleanup(mut self) -> Result<(), QuarryError> {
        match fs::remove_dir_all(&self.path) {
            Ok(()) => {
                self.active = false;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.active = false;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for SpillWorkspace {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_dir_all(&self.path);
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

    #[test]
    fn missing_workspaces_clean_up_and_invalid_names_create_nothing() {
        let parent = tempfile::tempdir().unwrap();
        for namespace in ["", "../escape", "/absolute", "a/b", "a\\b"] {
            assert!(SpillWorkspace::create_in(parent.path(), namespace).is_err());
        }
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
        let workspace = SpillWorkspace::create_in(parent.path(), "join").unwrap();
        fs::remove_dir(&workspace.path).unwrap();
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
