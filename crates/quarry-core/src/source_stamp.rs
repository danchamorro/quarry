use std::fs::{File, Metadata};
use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceStamp {
    len: u64,
    modified: Option<SystemTime>,
    readonly: bool,
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
    pub(crate) fn file_size(&self) -> u64 {
        self.len
    }

    pub(crate) fn from_file(file: &File) -> io::Result<Self> {
        #[cfg(target_os = "macos")]
        let before = data_generation(file);
        let stamp = Self::from_metadata(&file.metadata()?);
        #[cfg(target_os = "macos")]
        let stamp = Self {
            data_generation: before.filter(|_| before == data_generation(file)),
            ..stamp
        };
        Ok(stamp)
    }

    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            readonly: metadata.permissions().readonly(),
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

    /// Check the path's identity and ordinary metadata alongside matching a file
    /// descriptor. This alone does not establish that ctime changes are harmless.
    pub(crate) fn matches_metadata(&self, metadata: &Metadata) -> bool {
        self.same_file_metadata(&Self::from_metadata(metadata))
    }

    fn same_file_metadata(&self, other: &Self) -> bool {
        if self.len != other.len
            || self.modified != other.modified
            || self.readonly != other.readonly
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
            let stamp = SourceStamp {
                data_generation: Some(7),
                ..stamp.clone()
            };
            for (generation, expected) in [(Some(7), true), (Some(8), false), (Some(0), false)] {
                changed.data_generation = generation;
                assert_eq!(stamp.matches(&changed), expected);
            }
            changed = stamp.clone();
            changed.data_generation = Some(8);
            assert!(
                stamp.matches(&changed),
                "unchanged ctime retains existing behavior"
            );
        }
        #[cfg(not(target_os = "macos"))]
        assert!(!stamp.matches(&changed));
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
}
