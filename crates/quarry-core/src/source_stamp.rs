use std::fs::{File, Metadata};
use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;

/// Reopening a replaced input must not wait for a FIFO writer before its
/// type, version and cancellation checks can run. O_NONBLOCK leaves ordinary
/// file reads unchanged; validate the opened descriptor before using it.
pub(crate) fn open_regular_file(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    let file = File::from(rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    #[cfg(not(unix))]
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Select a regular file.",
        ));
    }
    Ok(file)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceStamp {
    len: u64,
    modified: Option<SystemTime>,
    readonly: bool,
    #[cfg(windows)]
    windows: Option<WindowsVersion>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanos: i64,
    #[cfg(target_os = "macos")]
    data_generation: Option<u32>,
}

impl SourceStamp {
    /// Spill ownership ignores size/time changes while its own writer appends.
    #[cfg(not(unix))]
    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        #[cfg(windows)]
        {
            self.windows.is_some()
                && self.windows.map(|v| v.identity) == other.windows.map(|v| v.identity)
        }
        #[cfg(not(windows))]
        {
            let _ = other;
            false
        }
    }

    pub(crate) fn file_size(&self) -> u64 {
        self.len
    }

    pub(crate) fn from_file(file: &File) -> io::Result<Self> {
        #[cfg(target_os = "macos")]
        let before = data_generation(file);
        #[cfg(windows)]
        let before = windows_version(file)?;
        let stamp = Self::from_metadata(&file.metadata()?);
        #[cfg(target_os = "macos")]
        let stamp = Self {
            data_generation: before.filter(|_| before == data_generation(file)),
            ..stamp
        };
        #[cfg(windows)]
        let stamp = {
            let after = windows_version(file)?;
            if before != after {
                return Err(io::Error::other(
                    "The file changed while its identity was being captured.",
                ));
            }
            Self {
                windows: Some(after),
                ..stamp
            }
        };
        Ok(stamp)
    }

    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            readonly: metadata.permissions().readonly(),
            #[cfg(windows)]
            windows: None,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.mode(),
            #[cfg(unix)]
            uid: metadata.uid(),
            #[cfg(unix)]
            gid: metadata.gid(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanos: metadata.ctime_nsec(),
            #[cfg(target_os = "macos")]
            data_generation: None,
        }
    }

    pub(crate) fn matches(&self, observed: &Self) -> bool {
        if !self.same_file_metadata(observed) {
            return false;
        }
        #[cfg(windows)]
        if self.windows.map(|version| version.changed)
            != observed.windows.map(|version| version.changed)
        {
            return false;
        }
        #[cfg(unix)]
        if (self.changed_seconds, self.changed_nanos)
            != (observed.changed_seconds, observed.changed_nanos)
        {
            // A matching data generation permits metadata-only ctime changes.
            // Unsupported filesystems and mapped files keep the stricter guard.
            #[cfg(target_os = "macos")]
            return matches!(
                (self.data_generation, observed.data_generation),
                (Some(before), Some(after)) if before != 0 && before == after
            );
            #[cfg(not(target_os = "macos"))]
            return false;
        }
        true
    }

    /// Publication can change ctime. None means that metadata-only change cannot
    /// be verified here, while false identifies a changed output, not a fallback.
    pub(crate) fn publication_match(&self, observed: &Self) -> Option<bool> {
        if self.matches(observed) {
            return Some(true);
        }
        if !self.same_file_metadata(observed) {
            return Some(false);
        }
        #[cfg(target_os = "macos")]
        if matches!(
            (self.data_generation, observed.data_generation),
            (Some(before), Some(after)) if before != 0 && after != 0
        ) {
            return Some(false);
        }
        None
    }

    /// Check the path's identity and ordinary metadata alongside matching a file
    /// descriptor. This alone does not establish that ctime changes are harmless.
    pub(crate) fn matches_metadata(&self, metadata: &Metadata) -> bool {
        let observed = Self::from_metadata(metadata);
        // Metadata alone has no stable Windows file ID. Callers must also
        // compare an opened path handle; source_matches_stamp does that.
        #[cfg(windows)]
        let observed = Self {
            windows: self.windows,
            ..observed
        };
        self.same_file_metadata(&observed)
    }

    fn same_file_metadata(&self, other: &Self) -> bool {
        if self.len != other.len
            || self.modified != other.modified
            || self.readonly != other.readonly
        {
            return false;
        }
        #[cfg(windows)]
        if self.windows.map(|version| version.identity)
            != other.windows.map(|version| version.identity)
        {
            return false;
        }
        #[cfg(unix)]
        if self.device != other.device
            || self.inode != other.inode
            || self.mode != other.mode
            || self.uid != other.uid
            || self.gid != other.gid
        {
            return false;
        }
        true
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowsVersion {
    identity: (u64, [u8; 16]),
    changed: i64,
}

#[cfg(windows)]
fn windows_version(file: &File) -> io::Result<WindowsVersion> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FILE_ID_INFO, FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut id = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
    let mut basic = std::mem::MaybeUninit::<FILE_BASIC_INFO>::uninit();
    // SAFETY: the file handle is live. Each buffer matches the requested
    // information class and has the exact initialized-on-success C layout.
    unsafe {
        if GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            id.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        if GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            basic.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let id = id.assume_init();
        let basic = basic.assume_init();
        Ok(WindowsVersion {
            identity: (id.VolumeSerialNumber, id.FileId.Identifier),
            changed: basic.ChangeTime,
        })
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn data_generation(file: &File) -> Option<u32> {
    let mut request = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_GEN_COUNT,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // getattrlist(2): length, five returned-attribute masks, then generation.
    let mut attributes = [0_u32; 7];
    // SAFETY: the descriptor is live, attrlist is initialized, and the writable
    // buffer has the required four-byte alignment and exactly the supplied size.
    let result = unsafe {
        libc::fgetattrlist(
            file.as_raw_fd(),
            std::ptr::from_mut(&mut request).cast(),
            attributes.as_mut_ptr().cast(),
            std::mem::size_of_val(&attributes),
            libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    if result != 0 {
        return None;
    }
    generation_from_attributes(&attributes)
}

#[cfg(target_os = "macos")]
fn generation_from_attributes(attributes: &[u32; 7]) -> Option<u32> {
    (attributes[0] as usize == std::mem::size_of_val(attributes)
        && attributes[1] & libc::ATTR_CMN_GEN_COUNT != 0
        && attributes[6] != 0)
        .then_some(attributes[6])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_metadata_changes_remain_guarded() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let stamp = SourceStamp::from_file(file.as_file()).unwrap();
        assert!(stamp.matches(&stamp));
        assert!(stamp.matches_metadata(&file.as_file().metadata().unwrap()));
        let mut changed = stamp.clone();
        changed.len += 1;
        assert!(!stamp.matches(&changed));
        assert_eq!(stamp.publication_match(&changed), Some(false));
        changed = stamp.clone();
        changed.readonly = !changed.readonly;
        assert!(!stamp.matches(&changed));
        #[cfg(unix)]
        {
            changed = stamp.clone();
            changed.inode = changed.inode.wrapping_add(1);
            assert!(!stamp.matches(&changed));
            changed = stamp.clone();
            changed.uid = changed.uid.wrapping_add(1);
            assert!(!stamp.matches(&changed));
            changed = stamp.clone();
            changed.gid = changed.gid.wrapping_add(1);
            assert!(!stamp.matches(&changed));
        }
    }

    #[cfg(unix)]
    #[test]
    fn chmod_is_rejected_even_when_readonly_is_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .unwrap();
        let stamp = SourceStamp::from_file(file.as_file()).unwrap();
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o640))
            .unwrap();
        let changed = SourceStamp::from_file(file.as_file()).unwrap();
        assert_eq!(stamp.readonly, changed.readonly);
        assert!(!stamp.matches(&changed));
    }

    #[cfg(unix)]
    #[test]
    fn changed_ctime_requires_valid_matching_generation() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let stamp = SourceStamp::from_file(file.as_file()).unwrap();
        let mut changed = stamp.clone();
        changed.changed_nanos = changed.changed_nanos.wrapping_add(1);
        #[cfg(target_os = "macos")]
        {
            changed.data_generation = None;
            assert!(!stamp.matches(&changed));
            assert_eq!(stamp.publication_match(&changed), None);
            let stamp = SourceStamp {
                data_generation: Some(7),
                ..stamp.clone()
            };
            for (generation, expected, publication) in [
                (Some(7), true, Some(true)),
                (Some(8), false, Some(false)),
                (Some(0), false, None),
            ] {
                changed.data_generation = generation;
                assert_eq!(stamp.matches(&changed), expected);
                assert_eq!(stamp.publication_match(&changed), publication);
            }
            changed = stamp.clone();
            changed.data_generation = Some(8);
            assert!(
                stamp.matches(&changed),
                "unchanged ctime retains existing behavior"
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(!stamp.matches(&changed));
            assert_eq!(stamp.publication_match(&changed), None);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn missing_zero_or_truncated_generation_is_unavailable() {
        let mut attributes = [28, libc::ATTR_CMN_GEN_COUNT, 0, 0, 0, 0, 7];
        assert_eq!(generation_from_attributes(&attributes), Some(7));
        attributes[6] = 0;
        assert_eq!(generation_from_attributes(&attributes), None);
        attributes[6] = 7;
        attributes[1] = 0;
        assert_eq!(generation_from_attributes(&attributes), None);
        attributes[1] = libc::ATTR_CMN_GEN_COUNT;
        attributes[0] = 24;
        assert_eq!(generation_from_attributes(&attributes), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn xattr_changes_preserve_stamp_but_restored_mtime_rewrites_do_not() {
        use std::io::{Seek, SeekFrom, Write};

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"Casey\n").unwrap();
        let stamp = SourceStamp::from_file(file.as_file()).unwrap();
        if stamp.data_generation.is_none() {
            eprintln!("metadata acceptance requires a valid macOS data generation count");
            return;
        }
        rustix::fs::fsetxattr(
            file.as_file(),
            "com.quarry.source-stamp-test",
            b"1",
            rustix::fs::XattrFlags::empty(),
        )
        .unwrap();
        let metadata_only = SourceStamp::from_file(file.as_file()).unwrap();
        assert!(stamp.matches(&metadata_only));
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"Other\n").unwrap();
        file.as_file()
            .set_times(std::fs::FileTimes::new().set_modified(stamp.modified.unwrap()))
            .unwrap();
        let rewritten = SourceStamp::from_file(file.as_file()).unwrap();
        assert_eq!(stamp.len, rewritten.len);
        assert_eq!(stamp.modified, rewritten.modified);
        assert!(!stamp.matches(&rewritten));
    }
    #[cfg(windows)]
    #[test]
    fn windows_identity_rejects_same_size_same_time_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.csv");
        std::fs::write(&path, b"id,name\n1,A\n").unwrap();
        let original = File::open(&path).unwrap();
        let stamp = SourceStamp::from_file(&original).unwrap();
        std::fs::rename(&path, dir.path().join("old.csv")).unwrap();
        std::fs::write(&path, b"name,id\nA,1\n").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp.modified.unwrap()))
            .unwrap();
        let replacement = File::open(&path).unwrap();
        assert!(
            stamp.matches_metadata(&replacement.metadata().unwrap()),
            "ordinary length/mtime/readonly metadata does not identify the selected input"
        );
        let observed = SourceStamp::from_file(&replacement).unwrap();
        assert!(!stamp.matches(&observed));
        assert!(!crate::export::source_matches_stamp(&original, &path, &stamp).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_change_time_rejects_restored_mtime_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.csv");
        std::fs::write(&path, b"id,name\n1,A\n").unwrap();
        let original = File::open(&path).unwrap();
        let stamp = SourceStamp::from_file(&original).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, b"name,id\nA,1\n").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp.modified.unwrap()))
            .unwrap();
        let observed = SourceStamp::from_file(&File::open(&path).unwrap()).unwrap();
        assert_eq!(stamp.len, observed.len);
        assert_eq!(stamp.modified, observed.modified);
        assert!(!stamp.matches(&observed));
        assert!(!crate::export::source_matches_stamp(&original, &path, &stamp).unwrap());
    }
}
