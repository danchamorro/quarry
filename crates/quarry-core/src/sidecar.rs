//! Hardened, privacy-preserving persistent sidecar structural index cache.
//!
//! Stores structural index metadata (checkpoint offsets, row counts, maximum width,
//! extended file identity, head/tail content hashes, and last-indexed-record hash)
//! in a private owner-only cache directory (`0700` dir, `0600` files) under keyed
//! opaque filenames. Never stores source filenames, column headers, or cell contents.
//!
//! All cached files are treated as untrusted input: any decode error, checksum
//! mismatch, identity mismatch, spot-check failure, unsupported volume (network or
//! FAT/exFAT), or unwritable cache directory silently falls back to normal indexing.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use quarry_delimited::{EscapeRule, ScannerDialect};

use crate::encoding::RecordScanner;
use crate::export::source_matches_stamp;
use crate::positional_io::read_at_exact_or_eof;
use crate::{Checkpoint, Encoding, IndexConfig, QuarryError, SourceStamp, StructuralIndex};

/// Default maximum total size of the persistent sidecar cache directory (256 MiB).
pub const DEFAULT_INDEX_CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Default minimum source file size before persisting or consulting a sidecar cache (4 MiB).
pub const DEFAULT_INDEX_CACHE_MIN_FILE_BYTES: u64 = 4 * 1024 * 1024;

const SIDECAR_MAGIC: &[u8; 4] = b"QIDX";
const SIDECAR_FORMAT_VERSION: u32 = 1;
const SIDECAR_HEADER_LEN: usize = 256;
const MAX_SIDECAR_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DECODED_CHECKPOINTS: usize = 1_048_576; // 16 MiB of 16-byte checkpoints
const CONTENT_HASH_WINDOW_BYTES: usize = 64 * 1024;
const SPOT_CHECK_MAX_COUNT: usize = 5;
const SPOT_CHECK_EXACT_SPAN_BYTES: u64 = 512 * 1024;
const SPOT_CHECK_FALLBACK_PROBE_BYTES: u64 = 64 * 1024;
const STALE_TEMP_AGE: Duration = Duration::from_secs(600);
const CACHE_KEY_FILENAME: &str = ".cache-key";
const CACHE_LOCK_FILENAME: &str = ".cache.lock";
const SIDECAR_EXT: &str = ".qidx";
const TEMP_EXT: &str = ".qidx.tmp";

fn is_quarry_sidecar_filename(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("idx-") else {
        return false;
    };
    let Some(hex) = rest.strip_suffix(SIDECAR_EXT) else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_quarry_temp_filename(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(".tmp-") else {
        return false;
    };
    let Some(body) = rest.strip_suffix(TEMP_EXT) else {
        return false;
    };
    let Some((pid, nonce)) = body.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && !nonce.is_empty()
        && nonce.bytes().all(|b| b.is_ascii_digit())
}

fn is_quarry_key_temp_filename(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(".cache-key.") else {
        return false;
    };
    let Some(body) = rest.strip_suffix(".tmp") else {
        return false;
    };
    let Some((pid, nonce)) = body.split_once('.') else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && !nonce.is_empty()
        && nonce.bytes().all(|b| b.is_ascii_digit())
}

fn reject_symlink_components(path: &Path) -> Result<(), QuarryError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        if matches!(
            component,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) => {
                // On macOS, `/var` and `/tmp` are OS-level root symlinks to `/private/*`.
                #[cfg(target_os = "macos")]
                if current == Path::new("/var") || current == Path::new("/tmp") {
                    continue;
                }
                if meta.file_type().is_symlink() {
                    return Err(QuarryError::InvalidOption(
                        "index cache path must not traverse or target a symlink",
                    ));
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
                    if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
                        return Err(QuarryError::InvalidOption(
                            "index cache path must not traverse or target a reparse point",
                        ));
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => break,
            Err(err) => return Err(QuarryError::Io(err)),
        }
    }
    Ok(())
}

const FLAG_BYTE_STRIDE: u32 = 1 << 0;
const FLAG_HAS_BIRTHTIME: u32 = 1 << 1;
const FLAG_HAS_DATA_GENERATION: u32 = 1 << 2;

static NEXT_TEMP_NONCE: AtomicU64 = AtomicU64::new(1);

/// Configuration for Quarry's persistent sidecar structural index cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCacheConfig {
    /// Whether persistent sidecar index loading and saving is enabled.
    pub enabled: bool,
    /// Optional explicit override for the cache directory (defaults to the OS private cache dir).
    pub cache_dir: Option<PathBuf>,
    /// Maximum total bytes kept in the cache directory, counting active temp writers.
    pub max_cache_bytes: u64,
    /// Minimum source file size (bytes) eligible for sidecar caching.
    pub min_file_bytes: u64,
}

impl Default for IndexCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cache_dir: None,
            max_cache_bytes: DEFAULT_INDEX_CACHE_MAX_BYTES,
            min_file_bytes: DEFAULT_INDEX_CACHE_MIN_FILE_BYTES,
        }
    }
}

impl IndexCacheConfig {
    /// Returns a configuration with sidecar caching disabled.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Returns an enabled configuration storing sidecars inside `cache_dir`.
    pub fn with_dir(cache_dir: PathBuf) -> Self {
        Self {
            enabled: true,
            cache_dir: Some(cache_dir),
            ..Self::default()
        }
    }
}

/// Summary returned by [`clear_index_cache`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheClearSummary {
    pub files_removed: u64,
    pub bytes_reclaimed: u64,
}

/// Resolves the default private cache directory for persistent structural sidecar indexes.
///
/// Honors `QUARRY_INDEX_CACHE_DIR` when non-empty; otherwise uses the OS user cache directory.
pub fn default_index_cache_dir() -> Option<PathBuf> {
    if let Some(override_dir) = std::env::var_os("QUARRY_INDEX_CACHE_DIR")
        && !override_dir.is_empty()
    {
        return Some(PathBuf::from(override_dir));
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME")
            && !home.is_empty()
        {
            return Some(
                PathBuf::from(home)
                    .join("Library")
                    .join("Caches")
                    .join("quarry")
                    .join("index-cache"),
            );
        }
        None
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME")
            && !xdg.is_empty()
        {
            return Some(PathBuf::from(xdg).join("quarry").join("index-cache"));
        }
        if let Some(home) = std::env::var_os("HOME")
            && !home.is_empty()
        {
            return Some(
                PathBuf::from(home)
                    .join(".cache")
                    .join("quarry")
                    .join("index-cache"),
            );
        }
        None
    }

    #[cfg(windows)]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA")
            && !local_app_data.is_empty()
        {
            return Some(
                PathBuf::from(local_app_data)
                    .join("quarry")
                    .join("index-cache"),
            );
        }
        None
    }

    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Removes all Quarry-managed sidecar index files, temporary staging files, and cache keys
/// from the specified (or default) index cache directory.
pub fn clear_index_cache(cache_dir: Option<&Path>) -> Result<CacheClearSummary, QuarryError> {
    let resolved = match cache_dir {
        Some(path) => path.to_path_buf(),
        None => match default_index_cache_dir() {
            Some(path) => path,
            None => return Ok(CacheClearSummary::default()),
        },
    };

    reject_symlink_components(&resolved)?;

    let meta = match fs::symlink_metadata(&resolved) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(CacheClearSummary::default());
        }
        Err(err) => return Err(QuarryError::Io(err)),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(QuarryError::InvalidOption(
            "index cache path must be a real directory, not a symlink",
        ));
    }

    // Only clear a directory verified to belong to the current user and be private.
    if !verify_existing_private_cache_dir(&resolved) {
        return Err(QuarryError::InvalidOption(
            "index cache path must be an owner-only private directory on a local filesystem",
        ));
    }

    let _lock = acquire_cache_lock(&resolved);

    let mut summary = CacheClearSummary::default();
    let entries = match fs::read_dir(&resolved) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(summary),
        Err(err) => return Err(QuarryError::Io(err)),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_managed = is_quarry_sidecar_filename(name)
            || is_quarry_temp_filename(name)
            || is_quarry_key_temp_filename(name)
            || name == CACHE_KEY_FILENAME;
        if !is_managed {
            continue;
        }
        let Ok(entry_meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if entry_meta.file_type().is_symlink() || !entry_meta.is_file() {
            continue;
        }
        let len = entry_meta.len();
        match fs::remove_file(&path) {
            Ok(()) => {
                summary.files_removed += 1;
                summary.bytes_reclaimed = summary.bytes_reclaimed.saturating_add(len);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(QuarryError::Io(err)),
        }
    }

    Ok(summary)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExtendedIdentity {
    platform_tag: u8,
    file_size: u64,
    mtime_secs: i64,
    mtime_nanos: u32,
    id_a: u64,
    id_b: u64,
    id_c: u64,
    ctime_secs: i64,
    ctime_nanos: i64,
    birthtime_secs: i64,
    birthtime_nanos: u32,
    has_birthtime: bool,
    data_generation: u32,
    has_data_generation: bool,
}

impl ExtendedIdentity {
    fn from_file(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sidecar requires a regular file",
            ));
        }
        let (mtime_secs, mtime_nanos) = system_time_to_parts(meta.modified()?);
        let (birthtime_secs, birthtime_nanos, has_birthtime) = match meta.created() {
            Ok(created) => {
                let (s, n) = system_time_to_parts(created);
                (s, n, true)
            }
            Err(_) => (0, 0, false),
        };

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            #[cfg(target_os = "macos")]
            let (data_generation, has_data_generation) =
                match crate::source_stamp::data_generation(file) {
                    Some(gen_val) => (gen_val, true),
                    None => (0, false),
                };
            #[cfg(not(target_os = "macos"))]
            let (data_generation, has_data_generation) = (0u32, false);

            Ok(Self {
                platform_tag: 1,
                file_size: meta.len(),
                mtime_secs,
                mtime_nanos,
                id_a: meta.dev(),
                id_b: meta.ino(),
                id_c: 0,
                ctime_secs: meta.ctime(),
                ctime_nanos: meta.ctime_nsec(),
                birthtime_secs,
                birthtime_nanos,
                has_birthtime,
                data_generation,
                has_data_generation,
            })
        }

        #[cfg(windows)]
        {
            let win = windows_extended_identity(file)?;
            Ok(Self {
                platform_tag: 2,
                file_size: meta.len(),
                mtime_secs,
                mtime_nanos,
                id_a: win.volume_serial,
                id_b: u64::from_le_bytes(win.file_id[0..8].try_into().unwrap()),
                id_c: u64::from_le_bytes(win.file_id[8..16].try_into().unwrap()),
                ctime_secs: win.change_time,
                ctime_nanos: 0,
                birthtime_secs: win.creation_time,
                birthtime_nanos: 0,
                has_birthtime: true,
                data_generation: 0,
                has_data_generation: false,
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "sidecar identity unsupported on this platform",
            ))
        }
    }

    fn matches(&self, other: &Self) -> bool {
        if self.platform_tag != other.platform_tag
            || self.file_size != other.file_size
            || self.mtime_secs != other.mtime_secs
            || self.mtime_nanos != other.mtime_nanos
            || self.id_a != other.id_a
            || self.id_b != other.id_b
            || self.id_c != other.id_c
            || self.ctime_secs != other.ctime_secs
            || self.ctime_nanos != other.ctime_nanos
        {
            return false;
        }
        if self.has_birthtime != other.has_birthtime {
            return false;
        }
        if self.has_birthtime
            && (self.birthtime_secs != other.birthtime_secs
                || self.birthtime_nanos != other.birthtime_nanos)
        {
            return false;
        }
        if self.has_data_generation != other.has_data_generation {
            return false;
        }
        if self.has_data_generation && self.data_generation != other.data_generation {
            return false;
        }
        true
    }
}

fn system_time_to_parts(time: SystemTime) -> (i64, u32) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs().min(i64::MAX as u64) as i64, d.subsec_nanos()),
        Err(err) => {
            let d = err.duration();
            (-(d.as_secs().min(i64::MAX as u64) as i64), d.subsec_nanos())
        }
    }
}

#[cfg(windows)]
struct WindowsExtendedIdentity {
    volume_serial: u64,
    file_id: [u8; 16],
    change_time: i64,
    creation_time: i64,
}

#[cfg(windows)]
fn windows_extended_identity(file: &File) -> io::Result<WindowsExtendedIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FILE_ID_INFO, FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut id = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
    let mut basic = std::mem::MaybeUninit::<FILE_BASIC_INFO>::uninit();
    // SAFETY: `file` is a valid open handle and the output structs match the exact Win32 ABI.
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
        Ok(WindowsExtendedIdentity {
            volume_serial: id.VolumeSerialNumber,
            file_id: id.FileId.Identifier,
            change_time: basic.ChangeTime,
            creation_time: basic.CreationTime,
        })
    }
}

/// Returns `true` only when `file` resides on a local, non-FAT/exFAT filesystem suitable
/// for persistent sidecar identity guarantees. Automatically disables sidecars on network
/// volumes (NFS, SMB/CIFS, AFP, WebDAV, FUSE, 9P, Ceph, remote drives) and FAT/exFAT.
pub(crate) fn is_eligible_local_filesystem(file: &File) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::zeroed();
        // SAFETY: `file` is a live file descriptor and `stat` is a valid writable `statfs`.
        let rc = unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) };
        if rc != 0 {
            return false;
        }
        // SAFETY: `fstatfs` returned 0, initializing `stat`.
        let stat = unsafe { stat.assume_init() };
        if (stat.f_flags & (libc::MNT_LOCAL as u32)) == 0 {
            return false;
        }
        let fstype_bytes: Vec<u8> = stat
            .f_fstypename
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        let Ok(fstype) = std::str::from_utf8(&fstype_bytes) else {
            return false;
        };
        is_allowed_macos_fstype(fstype)
    }

    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = rustix::fs::fstatfs(file) else {
            return false;
        };
        is_allowed_linux_fs_magic(stat.f_type as u64)
    }

    #[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
    {
        let _ = file;
        false
    }

    #[cfg(windows)]
    {
        is_eligible_windows_filesystem(file)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        false
    }
}

#[cfg(any(target_os = "macos", test))]
fn is_allowed_macos_fstype(fstype: &str) -> bool {
    let lower = fstype.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "" | "msdos"
            | "exfat"
            | "vfat"
            | "fat"
            | "fat16"
            | "fat32"
            | "smbfs"
            | "nfs"
            | "afpfs"
            | "webdav"
            | "cifs"
            | "fuse"
            | "osxfuse"
            | "macfuse"
            | "fusefs"
            | "ntfs"
    )
}

#[cfg(any(target_os = "linux", test))]
fn is_allowed_linux_fs_magic(magic: u64) -> bool {
    const MSDOS_SUPER_MAGIC: u64 = 0x4d44;
    const EXFAT_SUPER_MAGIC: u64 = 0x2011_bab0;
    const NFS_SUPER_MAGIC: u64 = 0x6969;
    const SMB_SUPER_MAGIC: u64 = 0x517b;
    const SMB2_SUPER_MAGIC: u64 = 0xfe53_4d42;
    const CIFS_SUPER_MAGIC: u64 = 0xff53_4d42;
    const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;
    const CEPH_SUPER_MAGIC: u64 = 0x00c3_6400;
    const NINEP_MAGIC: u64 = 0x0102_1997;
    const AFS_SUPER_MAGIC: u64 = 0x5346_414f;
    const CODA_SUPER_MAGIC: u64 = 0x7375_7245;
    const NCP_SUPER_MAGIC: u64 = 0x564c;

    !matches!(
        magic,
        MSDOS_SUPER_MAGIC
            | EXFAT_SUPER_MAGIC
            | NFS_SUPER_MAGIC
            | SMB_SUPER_MAGIC
            | SMB2_SUPER_MAGIC
            | CIFS_SUPER_MAGIC
            | FUSE_SUPER_MAGIC
            | CEPH_SUPER_MAGIC
            | NINEP_MAGIC
            | AFS_SUPER_MAGIC
            | CODA_SUPER_MAGIC
            | NCP_SUPER_MAGIC
    )
}

#[cfg(windows)]
fn is_eligible_windows_filesystem(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetFinalPathNameByHandleW, GetVolumeInformationByHandleW,
    };

    const DRIVE_FIXED: u32 = 3;

    let mut fs_name = [0u16; 64];
    // SAFETY: `file` is a valid handle and `fs_name` has length 64.
    let vol_ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if vol_ok == 0 {
        return false;
    }
    let nul_pos = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    let fs_str = String::from_utf16_lossy(&fs_name[..nul_pos]).to_ascii_uppercase();
    if matches!(
        fs_str.as_str(),
        "" | "FAT" | "FAT32" | "EXFAT" | "CDFS" | "UDF"
    ) {
        return false;
    }

    let mut path_buf = [0u16; 512];
    // SAFETY: `file` is a valid handle and `path_buf` has length 512.
    let len = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            path_buf.as_mut_ptr(),
            path_buf.len() as u32,
            0,
        )
    } as usize;
    if len == 0 || len >= path_buf.len() {
        return false;
    }
    let final_path = String::from_utf16_lossy(&path_buf[..len]);
    let upper = final_path.to_ascii_uppercase();
    if upper.starts_with(r"\\?\UNC\") || upper.starts_with(r"\\") && !upper.starts_with(r"\\?\") {
        return false;
    }
    let drive_letter = upper
        .strip_prefix(r"\\?\")
        .unwrap_or(&upper)
        .as_bytes()
        .first()
        .copied();
    let Some(letter) = drive_letter.filter(|b| b.is_ascii_alphabetic()) else {
        return false;
    };
    let root: [u16; 4] = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
    // SAFETY: `root` is a NUL-terminated UTF-16 drive root string.
    let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
    drive_type == DRIVE_FIXED
}

fn encoding_to_tag(encoding: Encoding) -> u8 {
    match encoding {
        Encoding::Utf8 => 1,
        Encoding::Utf16Le => 2,
        Encoding::Utf16Be => 3,
        Encoding::Windows1252 => 4,
        Encoding::Latin1 => 5,
    }
}

fn dialect_to_tags(dialect: ScannerDialect) -> (u8, u8, u8, u8) {
    let quote = dialect.quote.unwrap_or(0);
    let (escape_kind, escape_byte) = match dialect.escape {
        EscapeRule::DoubledQuote => (1, 0),
        EscapeRule::EscapeByte(b) => (2, b),
    };
    (dialect.delimiter, quote, escape_kind, escape_byte)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedSidecar {
    identity: ExtendedIdentity,
    encoding_tag: u8,
    delimiter: u8,
    quote: u8,
    escape_kind: u8,
    escape_byte: u8,
    byte_stride_checkpoints: bool,
    head_sha256: [u8; 32],
    tail_sha256: [u8; 32],
    last_record_sha256: [u8; 32],
    indexed_rows: u64,
    indexed_bytes: u64,
    maximum_width: u64,
    checkpoint_every: u64,
    checkpoint_byte_stride: Option<u64>,
    checkpoints: Vec<Checkpoint>,
}

fn encode_sidecar_bytes(
    identity: &ExtendedIdentity,
    encoding: Encoding,
    dialect: ScannerDialect,
    head_sha256: &[u8; 32],
    tail_sha256: &[u8; 32],
    last_record_sha256: &[u8; 32],
    index: &StructuralIndex,
) -> Option<Vec<u8>> {
    let checkpoints = index.checkpoints();
    if checkpoints.is_empty() || checkpoints.len() > MAX_DECODED_CHECKPOINTS {
        return None;
    }
    let total_len = SIDECAR_HEADER_LEN.checked_add(checkpoints.len().checked_mul(16)?)?;
    if total_len as u64 > MAX_SIDECAR_FILE_BYTES {
        return None;
    }

    let mut flags = 0u32;
    if index.telemetry().byte_stride_checkpoints {
        flags |= FLAG_BYTE_STRIDE;
    }
    if identity.has_birthtime {
        flags |= FLAG_HAS_BIRTHTIME;
    }
    if identity.has_data_generation {
        flags |= FLAG_HAS_DATA_GENERATION;
    }

    let (delimiter, quote, escape_kind, escape_byte) = dialect_to_tags(dialect);
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(SIDECAR_MAGIC); // 0..4
    out.extend_from_slice(&SIDECAR_FORMAT_VERSION.to_le_bytes()); // 4..8
    out.extend_from_slice(&flags.to_le_bytes()); // 8..12
    out.extend_from_slice(&0u32.to_le_bytes()); // 12..16 (CRC placeholder)
    out.push(identity.platform_tag); // 16
    out.push(encoding_to_tag(encoding)); // 17
    out.push(delimiter); // 18
    out.push(quote); // 19
    out.push(escape_kind); // 20
    out.push(escape_byte); // 21
    out.extend_from_slice(&[0u8; 2]); // 22..24
    out.extend_from_slice(&identity.file_size.to_le_bytes()); // 24..32
    out.extend_from_slice(&identity.mtime_secs.to_le_bytes()); // 32..40
    out.extend_from_slice(&identity.mtime_nanos.to_le_bytes()); // 40..44
    out.extend_from_slice(&0u32.to_le_bytes()); // 44..48
    out.extend_from_slice(&identity.id_a.to_le_bytes()); // 48..56
    out.extend_from_slice(&identity.id_b.to_le_bytes()); // 56..64
    out.extend_from_slice(&identity.id_c.to_le_bytes()); // 64..72
    out.extend_from_slice(&identity.ctime_secs.to_le_bytes()); // 72..80
    out.extend_from_slice(&identity.ctime_nanos.to_le_bytes()); // 80..88
    out.extend_from_slice(&identity.birthtime_secs.to_le_bytes()); // 88..96
    out.extend_from_slice(&identity.birthtime_nanos.to_le_bytes()); // 96..100
    out.extend_from_slice(&identity.data_generation.to_le_bytes()); // 100..104
    out.extend_from_slice(head_sha256); // 104..136
    out.extend_from_slice(tail_sha256); // 136..168
    out.extend_from_slice(last_record_sha256); // 168..200
    out.extend_from_slice(&index.indexed_rows().to_le_bytes()); // 200..208
    out.extend_from_slice(&index.indexed_bytes().to_le_bytes()); // 208..216
    out.extend_from_slice(&(index.maximum_width() as u64).to_le_bytes()); // 216..224
    out.extend_from_slice(&index.checkpoint_every().to_le_bytes()); // 224..232
    out.extend_from_slice(&index.checkpoint_byte_stride().unwrap_or(0).to_le_bytes()); // 232..240
    out.extend_from_slice(&(checkpoints.len() as u32).to_le_bytes()); // 240..244
    out.extend_from_slice(&[0u8; 12]); // 244..256 keyed MAC slot (filled after CRC)
    debug_assert_eq!(out.len(), SIDECAR_HEADER_LEN);

    for cp in checkpoints {
        out.extend_from_slice(&cp.row.to_le_bytes());
        out.extend_from_slice(&cp.offset.to_le_bytes());
    }

    let crc = crc32c(&out[16..]);
    out[12..16].copy_from_slice(&crc.to_le_bytes());
    Some(out)
}

fn compute_sidecar_mac(key: &[u8; 32], encoded: &[u8]) -> [u8; 12] {
    let mut hasher = Sha256::new();
    hasher.update(b"quarry-sidecar-mac-v1:");
    hasher.update(key);
    hasher.update(b"\0");
    hasher.update(&encoded[0..12]);
    hasher.update(&encoded[16..244]);
    hasher.update(&encoded[SIDECAR_HEADER_LEN..]);
    let digest = hasher.finalize();
    let mut tag = [0u8; 12];
    tag.copy_from_slice(&digest[..12]);
    tag
}

fn seal_sidecar_mac(key: &[u8; 32], encoded: &mut [u8]) {
    let tag = compute_sidecar_mac(key, encoded);
    encoded[244..256].copy_from_slice(&tag);
    let crc = crc32c(&encoded[16..]);
    encoded[12..16].copy_from_slice(&crc.to_le_bytes());
}

fn verify_sidecar_mac(key: &[u8; 32], encoded: &[u8]) -> bool {
    if encoded.len() < SIDECAR_HEADER_LEN {
        return false;
    }
    let expected = compute_sidecar_mac(key, encoded);
    // Constant-time comparison over the 12-byte MAC tag.
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(&encoded[244..256]) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Decodes an untrusted sidecar byte buffer. Never panics on any input.
pub(crate) fn decode_sidecar_bytes(bytes: &[u8]) -> Option<DecodedSidecar> {
    if bytes.len() < SIDECAR_HEADER_LEN || (bytes.len() as u64) > MAX_SIDECAR_FILE_BYTES {
        return None;
    }
    if &bytes[0..4] != SIDECAR_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != SIDECAR_FORMAT_VERSION {
        return None;
    }
    let flags = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let allowed_flags = FLAG_BYTE_STRIDE | FLAG_HAS_BIRTHTIME | FLAG_HAS_DATA_GENERATION;
    if (flags & !allowed_flags) != 0 {
        return None;
    }
    let expected_crc = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
    if crc32c(&bytes[16..]) != expected_crc {
        return None;
    }

    let platform_tag = bytes[16];
    let encoding_tag = bytes[17];
    let delimiter = bytes[18];
    let quote = bytes[19];
    let escape_kind = bytes[20];
    let escape_byte = bytes[21];

    let file_size = u64::from_le_bytes(bytes[24..32].try_into().ok()?);
    let mtime_secs = i64::from_le_bytes(bytes[32..40].try_into().ok()?);
    let mtime_nanos = u32::from_le_bytes(bytes[40..44].try_into().ok()?);
    let id_a = u64::from_le_bytes(bytes[48..56].try_into().ok()?);
    let id_b = u64::from_le_bytes(bytes[56..64].try_into().ok()?);
    let id_c = u64::from_le_bytes(bytes[64..72].try_into().ok()?);
    let ctime_secs = i64::from_le_bytes(bytes[72..80].try_into().ok()?);
    let ctime_nanos = i64::from_le_bytes(bytes[80..88].try_into().ok()?);
    let birthtime_secs = i64::from_le_bytes(bytes[88..96].try_into().ok()?);
    let birthtime_nanos = u32::from_le_bytes(bytes[96..100].try_into().ok()?);
    let data_generation = u32::from_le_bytes(bytes[100..104].try_into().ok()?);

    let mut head_sha256 = [0u8; 32];
    head_sha256.copy_from_slice(&bytes[104..136]);
    let mut tail_sha256 = [0u8; 32];
    tail_sha256.copy_from_slice(&bytes[136..168]);
    let mut last_record_sha256 = [0u8; 32];
    last_record_sha256.copy_from_slice(&bytes[168..200]);

    let indexed_rows = u64::from_le_bytes(bytes[200..208].try_into().ok()?);
    let indexed_bytes = u64::from_le_bytes(bytes[208..216].try_into().ok()?);
    let maximum_width = u64::from_le_bytes(bytes[216..224].try_into().ok()?);
    let checkpoint_every = u64::from_le_bytes(bytes[224..232].try_into().ok()?);
    let raw_stride = u64::from_le_bytes(bytes[232..240].try_into().ok()?);
    let checkpoint_count = u32::from_le_bytes(bytes[240..244].try_into().ok()?) as usize;

    if checkpoint_count == 0 || checkpoint_count > MAX_DECODED_CHECKPOINTS {
        return None;
    }
    if checkpoint_every == 0 {
        return None;
    }
    let expected_len = SIDECAR_HEADER_LEN.checked_add(checkpoint_count.checked_mul(16)?)?;
    if bytes.len() != expected_len {
        return None;
    }
    if indexed_bytes != file_size {
        return None;
    }
    if (indexed_rows == 0) != (maximum_width == 0) {
        return None;
    }
    let _ = usize::try_from(maximum_width).ok()?;

    let mut checkpoints = Vec::with_capacity(checkpoint_count);
    let mut offset = SIDECAR_HEADER_LEN;
    for i in 0..checkpoint_count {
        let row = u64::from_le_bytes(bytes[offset..offset + 8].try_into().ok()?);
        let cp_offset = u64::from_le_bytes(bytes[offset + 8..offset + 16].try_into().ok()?);
        offset += 16;

        if i == 0 {
            if row != 0 || cp_offset != 0 {
                return None;
            }
        } else {
            let prev: Checkpoint = checkpoints[i - 1];
            if row <= prev.row || cp_offset <= prev.offset {
                return None;
            }
        }
        if row > indexed_rows || cp_offset > file_size {
            return None;
        }
        checkpoints.push(Checkpoint {
            row,
            offset: cp_offset,
        });
    }

    let byte_stride_checkpoints = (flags & FLAG_BYTE_STRIDE) != 0;
    let checkpoint_byte_stride = if raw_stride == 0 {
        if byte_stride_checkpoints {
            return None;
        }
        None
    } else {
        Some(raw_stride)
    };

    Some(DecodedSidecar {
        identity: ExtendedIdentity {
            platform_tag,
            file_size,
            mtime_secs,
            mtime_nanos,
            id_a,
            id_b,
            id_c,
            ctime_secs,
            ctime_nanos,
            birthtime_secs,
            birthtime_nanos,
            has_birthtime: (flags & FLAG_HAS_BIRTHTIME) != 0,
            data_generation,
            has_data_generation: (flags & FLAG_HAS_DATA_GENERATION) != 0,
        },
        encoding_tag,
        delimiter,
        quote,
        escape_kind,
        escape_byte,
        byte_stride_checkpoints,
        head_sha256,
        tail_sha256,
        last_record_sha256,
        indexed_rows,
        indexed_bytes,
        maximum_width,
        checkpoint_every,
        checkpoint_byte_stride,
        checkpoints,
    })
}

fn compute_head_tail_hashes(file: &File, file_size: u64) -> io::Result<([u8; 32], [u8; 32])> {
    let head_len = file_size.min(CONTENT_HASH_WINDOW_BYTES as u64) as usize;
    let mut buf = vec![0u8; head_len];
    if head_len > 0 && read_at_exact_or_eof(file, 0, &mut buf)? != head_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short head read",
        ));
    }
    let head_hash = Sha256::digest(&buf);

    let tail_offset = file_size.saturating_sub(CONTENT_HASH_WINDOW_BYTES as u64);
    let tail_len = (file_size - tail_offset) as usize;
    if tail_offset == 0 {
        return Ok((head_hash, head_hash));
    }
    buf.resize(tail_len, 0);
    if read_at_exact_or_eof(file, tail_offset, &mut buf)? != tail_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short tail read",
        ));
    }
    let tail_hash = Sha256::digest(&buf);
    Ok((head_hash, tail_hash))
}

/// Computes a privacy-preserving SHA-256 hash of the last indexed record's bounded tail
/// (for future Live Watch extensibility) without storing any raw cell bytes.
fn compute_last_record_hash(
    file: &File,
    file_size: u64,
    checkpoints: &[Checkpoint],
) -> io::Result<[u8; 32]> {
    let last_cp_offset = checkpoints.last().map_or(0, |cp| cp.offset);
    let span = file_size.saturating_sub(last_cp_offset);
    let read_len = span.min(CONTENT_HASH_WINDOW_BYTES as u64) as usize;
    let read_start = file_size.saturating_sub(read_len as u64);
    let mut buf = vec![0u8; read_len];
    if read_len > 0 && read_at_exact_or_eof(file, read_start, &mut buf)? != read_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short last-record read",
        ));
    }
    let mut hasher = Sha256::new();
    hasher.update(b"quarry-last-record-v1:");
    hasher.update(&last_cp_offset.to_le_bytes());
    hasher.update(&file_size.to_le_bytes());
    hasher.update(&buf);
    Ok(hasher.finalize())
}

fn spot_check_checkpoints(
    file: &File,
    file_size: u64,
    indexed_rows: u64,
    delimiter: u8,
    encoding: Encoding,
    checkpoints: &[Checkpoint],
) -> bool {
    if checkpoints.len() <= 1 {
        return true;
    }
    let non_zero_count = checkpoints.len() - 1;
    let mut indices = Vec::with_capacity(SPOT_CHECK_MAX_COUNT);
    if non_zero_count <= SPOT_CHECK_MAX_COUNT {
        for i in 1..checkpoints.len() {
            indices.push(i);
        }
    } else {
        for slot in 0..SPOT_CHECK_MAX_COUNT {
            let idx = 1 + (slot * (non_zero_count - 1)) / (SPOT_CHECK_MAX_COUNT - 1);
            if indices.last().copied() != Some(idx) {
                indices.push(idx);
            }
        }
    }

    for idx in indices {
        let cp = checkpoints[idx];
        if cp.offset == 0 || cp.offset > file_size {
            return false;
        }
        // A valid file without a trailing newline may emit its final checkpoint at EOF (`file_size`).
        if cp.offset == file_size {
            if cp.row != indexed_rows {
                return false;
            }
            continue;
        }
        // Non-EOF checkpoints must sit immediately after a literal LF newline.
        if !preceded_by_lf(file, cp.offset, encoding) {
            return false;
        }

        let next_cp = checkpoints.get(idx + 1).copied();
        let exact_next = next_cp.filter(|next| {
            next.offset > cp.offset
                && next.offset <= file_size
                && (next.offset - cp.offset) <= SPOT_CHECK_EXACT_SPAN_BYTES
        });

        let mut scanner = match RecordScanner::at_offset(delimiter, cp.offset, encoding) {
            Ok(s) => s,
            Err(_) => return false,
        };

        if let Some(next) = exact_next {
            let span = (next.offset - cp.offset) as usize;
            let mut buf = vec![0u8; span];
            match read_at_exact_or_eof(file, cp.offset, &mut buf) {
                Ok(n) if n == span => {}
                _ => return false,
            }
            let mut rows = 0u64;
            let mut last_end = cp.offset;
            if scanner
                .scan_chunk(&buf, cp.offset, |end| {
                    rows += 1;
                    last_end = end;
                })
                .is_err()
            {
                return false;
            }
            if next.offset == file_size {
                match scanner.finish(file_size, |end| {
                    rows += 1;
                    last_end = end;
                }) {
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
            if rows != next.row.saturating_sub(cp.row) || last_end != next.offset {
                return false;
            }
        } else {
            let probe_len = (file_size - cp.offset).min(SPOT_CHECK_FALLBACK_PROBE_BYTES) as usize;
            let aligned_len = match encoding {
                Encoding::Utf16Le | Encoding::Utf16Be => probe_len & !1,
                _ => probe_len,
            };
            if aligned_len == 0 {
                continue;
            }
            let mut buf = vec![0u8; aligned_len];
            match read_at_exact_or_eof(file, cp.offset, &mut buf) {
                Ok(n) if n == aligned_len => {}
                _ => return false,
            }
            if scanner.scan_chunk(&buf, cp.offset, |_| {}).is_err() {
                return false;
            }
            if cp.offset + aligned_len as u64 == file_size
                && scanner.finish(file_size, |_| {}).is_err()
            {
                return false;
            }
        }
    }

    true
}

fn preceded_by_lf(file: &File, offset: u64, encoding: Encoding) -> bool {
    match encoding {
        Encoding::Utf16Le => {
            if offset < 2 {
                return false;
            }
            let mut prev = [0u8; 2];
            read_at_exact_or_eof(file, offset - 2, &mut prev).is_ok_and(|n| n == 2)
                && prev == [b'\n', 0]
        }
        Encoding::Utf16Be => {
            if offset < 2 {
                return false;
            }
            let mut prev = [0u8; 2];
            read_at_exact_or_eof(file, offset - 2, &mut prev).is_ok_and(|n| n == 2)
                && prev == [0, b'\n']
        }
        Encoding::Utf8 | Encoding::Windows1252 | Encoding::Latin1 => {
            if offset < 1 {
                return false;
            }
            let mut prev = [0u8; 1];
            read_at_exact_or_eof(file, offset - 1, &mut prev).is_ok_and(|n| n == 1)
                && prev[0] == b'\n'
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_has_empty_acl(file: &File) -> bool {
    use std::ffi::c_void;
    use std::os::fd::AsRawFd;

    const ACL_FIRST_ENTRY: libc::c_int = 0;

    unsafe extern "C" {
        fn acl_get_fd(fd: libc::c_int) -> *mut c_void;
        fn acl_get_entry(
            acl: *mut c_void,
            entry_id: libc::c_int,
            entry_p: *mut *mut c_void,
        ) -> libc::c_int;
        fn acl_free(obj_p: *mut c_void) -> libc::c_int;
    }

    unsafe {
        let acl = acl_get_fd(file.as_raw_fd());
        if acl.is_null() {
            let err = io::Error::last_os_error().raw_os_error();
            return matches!(
                err,
                Some(libc::ENOENT) | Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP)
            );
        }
        let mut entry: *mut c_void = std::ptr::null_mut();
        let rc = acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry);
        acl_free(acl);
        rc != 0
    }
}

/// Verifies an existing directory is owner-only, non-symlink, and on a local filesystem
/// without modifying its permissions or ACLs.
fn verify_existing_private_cache_dir(dir: &Path) -> bool {
    if reject_symlink_components(dir).is_err() {
        return false;
    }
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return false;
    }

    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};
        use std::os::unix::fs::MetadataExt;

        let Ok(fd) = rustix::fs::open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            return false;
        };
        let dir_file = File::from(fd);
        let Ok(fmeta) = dir_file.metadata() else {
            return false;
        };
        let uid = rustix::process::getuid().as_raw();
        if !fmeta.is_dir()
            || fmeta.uid() != uid
            || (fmeta.mode() & 0o077) != 0
            || !is_eligible_local_filesystem(&dir_file)
        {
            return false;
        }
        #[cfg(target_os = "macos")]
        if !macos_has_empty_acl(&dir_file) {
            return false;
        }
        true
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        const READ_CONTROL: u32 = 0x0002_0000;
        if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return false;
        }
        let Ok(dir_file) = fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(dir)
        else {
            return false;
        };
        let Ok(dir_meta) = dir_file.metadata() else {
            return false;
        };
        dir_meta.is_dir()
            && (dir_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) == 0
            && is_eligible_windows_filesystem(&dir_file)
            && verify_windows_owner_only_dacl(dir_file.as_raw_handle())
    }

    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

fn resolve_and_harden_cache_dir(config: &IndexCacheConfig) -> Option<PathBuf> {
    let dir = config.cache_dir.clone().or_else(default_index_cache_dir)?;

    reject_symlink_components(&dir).ok()?;

    let existed = fs::symlink_metadata(&dir).is_ok();
    if existed && config.cache_dir.is_some() {
        // Never mutate permissions/ACLs on an existing user-selected directory;
        // require it to already be strictly owner-only on a local filesystem.
        return verify_existing_private_cache_dir(&dir).then_some(dir);
    }

    if !existed && create_private_dir_all(&dir).is_err() {
        return None;
    }

    let meta = fs::symlink_metadata(&dir).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }

    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let fd = rustix::fs::open(
            &dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?;
        let dir_file = File::from(fd);
        let fmeta = dir_file.metadata().ok()?;
        let uid = rustix::process::getuid().as_raw();
        if !fmeta.is_dir() || fmeta.uid() != uid || !is_eligible_local_filesystem(&dir_file) {
            return None;
        }
        if (fmeta.mode() & 0o077) != 0 {
            let _ = rustix::fs::fchmod(&dir_file, Mode::from_raw_mode(0o700));
            let recheck = dir_file.metadata().ok()?;
            if !recheck.is_dir() || recheck.uid() != uid || (recheck.mode() & 0o077) != 0 {
                let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
                let recheck2 = dir_file.metadata().ok()?;
                if !recheck2.is_dir() || recheck2.uid() != uid || (recheck2.mode() & 0o077) != 0 {
                    return None;
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            // Clear any inherited ACL grants so cache files remain strictly owner-only.
            crate::storage::set_macos_acl(&dir_file, None).ok()?;
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        const READ_CONTROL: u32 = 0x0002_0000;
        const WRITE_DAC: u32 = 0x0004_0000;
        if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return None;
        }
        let dir_file = fs::OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&dir)
            .ok()?;
        let dir_meta = dir_file.metadata().ok()?;
        if !dir_meta.is_dir()
            || (dir_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            || !is_eligible_windows_filesystem(&dir_file)
        {
            return None;
        }
        // Apply and verify a protected owner-only DACL (current user + SYSTEM) stripping
        // any inherited grants.
        if set_windows_owner_only_dacl(dir_file.as_raw_handle()).is_err()
            || !verify_windows_owner_only_dacl(dir_file.as_raw_handle())
        {
            return None;
        }
    }

    Some(dir)
}

fn acquire_cache_lock(cache_dir: &Path) -> Option<File> {
    let lock_path = cache_dir.join(CACHE_LOCK_FILENAME);

    #[cfg(unix)]
    {
        use rustix::fs::{FlockOperation, Mode, OFlags};
        use std::os::unix::fs::MetadataExt;

        let fd = rustix::fs::open(
            &lock_path,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )
        .ok()?;
        let file = File::from(fd);
        let meta = file.metadata().ok()?;
        if !meta.is_file()
            || meta.uid() != rustix::process::getuid().as_raw()
            || (meta.mode() & 0o077) != 0
        {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            crate::storage::set_macos_acl(&file, None).ok()?;
        }
        rustix::fs::flock(&file, FlockOperation::LockExclusive).ok()?;
        Some(file)
    }

    #[cfg(windows)]
    {
        use std::ffi::c_void;
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
            FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
        #[repr(C)]
        struct Overlapped {
            internal: usize,
            internal_high: usize,
            offset: u32,
            offset_high: u32,
            h_event: *mut c_void,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn LockFileEx(
                file: *mut c_void,
                flags: u32,
                reserved: u32,
                bytes_low: u32,
                bytes_high: u32,
                overlapped: *mut Overlapped,
            ) -> i32;
        }

        let file = fs::OpenOptions::new()
            .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .create(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&lock_path)
            .ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file()
            || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            || !verify_windows_owner_only_dacl(file.as_raw_handle())
        {
            return None;
        }
        let mut overlapped = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: std::ptr::null_mut(),
        };
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(file)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = lock_path;
        None
    }
}

fn load_or_create_cache_key(cache_dir: &Path) -> Option<[u8; 32]> {
    let key_path = cache_dir.join(CACHE_KEY_FILENAME);
    if let Some(key) = read_verified_key(&key_path) {
        return Some(key);
    }

    let _lock = acquire_cache_lock(cache_dir)?;
    if let Some(key) = read_verified_key(&key_path) {
        return Some(key);
    }

    // Check if an existing entry at `.cache-key` is a recoverable owner-owned partial file.
    let mut recover_partial = false;
    match fs::symlink_metadata(&key_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.uid() != rustix::process::getuid().as_raw() || (meta.mode() & 0o077) != 0 {
                    return None;
                }
            }
            if meta.len() == 32 {
                // Valid length failed `read_verified_key` (e.g. unreadable/ACL failure): do not clobber.
                return None;
            }
            recover_partial = true;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }

    let new_key = generate_random_key();
    let mut tmp_path = None;
    let mut tmp_file = None;
    for _ in 0..8 {
        let nonce = NEXT_TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let candidate = cache_dir.join(format!(".cache-key.{}.{nonce}.tmp", std::process::id()));
        match create_private_exclusive_file(&candidate) {
            Ok(file) => {
                tmp_path = Some(candidate);
                tmp_file = Some(file);
                break;
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    let tmp_path = tmp_path?;
    let mut tmp_file = tmp_file?;
    let write_res = tmp_file
        .write_all(&new_key)
        .and_then(|()| tmp_file.sync_all());
    drop(tmp_file);
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
        return None;
    }

    if recover_partial {
        if fs::rename(&tmp_path, &key_path).is_err() {
            let _ = fs::remove_file(&tmp_path);
            return None;
        }
        return Some(new_key);
    }

    match fs::hard_link(&tmp_path, &key_path) {
        Ok(()) => {
            let _ = fs::remove_file(&tmp_path);
            Some(new_key)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&tmp_path);
            read_verified_key(&key_path)
        }
        Err(_) => {
            // Fallback for filesystems without hard-link support (we hold `_lock` and verified NotFound).
            let res = fs::rename(&tmp_path, &key_path);
            if res.is_err() {
                let _ = fs::remove_file(&tmp_path);
                return None;
            }
            Some(new_key)
        }
    }
}

fn read_verified_key(key_path: &Path) -> Option<[u8; 32]> {
    let mut file = open_private_readonly_nofollow(key_path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() != 32 {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != rustix::process::getuid().as_raw() || (meta.mode() & 0o077) != 0 {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            crate::storage::set_macos_acl(&file, None).ok()?;
        }
    }
    let mut key = [0u8; 32];
    file.read_exact(&mut key).ok()?;
    Some(key)
}

fn generate_random_key() -> [u8; 32] {
    #[cfg(unix)]
    {
        if let Ok(mut urandom) = File::open("/dev/urandom") {
            let mut key = [0u8; 32];
            if urandom.read_exact(&mut key).is_ok() {
                return key;
            }
        }
    }

    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut hasher = Sha256::new();
    for round in 0..8u64 {
        let state = RandomState::new();
        let mut h = state.build_hasher();
        h.write_u64(round);
        h.write_u32(std::process::id());
        hasher.update(&h.finish().to_le_bytes());
    }
    let (secs, nanos) = system_time_to_parts(SystemTime::now());
    hasher.update(&secs.to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hasher.update(
        &NEXT_TEMP_NONCE
            .fetch_add(1, Ordering::Relaxed)
            .to_le_bytes(),
    );
    hasher.finalize()
}

fn keyed_sidecar_filename(
    key: &[u8; 32],
    path: &Path,
    identity: &ExtendedIdentity,
    encoding: Encoding,
    dialect: ScannerDialect,
    requested_checkpoint_every: u64,
) -> String {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (delimiter, quote, escape_kind, escape_byte) = dialect_to_tags(dialect);

    let mut hasher = Sha256::new();
    hasher.update(b"quarry-sidecar-key-v1:");
    hasher.update(key);
    hasher.update(b"\0");
    hasher.update(canonical.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(&[
        identity.platform_tag,
        encoding_to_tag(encoding),
        delimiter,
        quote,
        escape_kind,
        escape_byte,
    ]);
    hasher.update(&identity.id_a.to_le_bytes());
    hasher.update(&identity.id_b.to_le_bytes());
    hasher.update(&identity.id_c.to_le_bytes());
    hasher.update(&requested_checkpoint_every.to_le_bytes());
    let digest = hasher.finalize();

    let mut name = String::with_capacity(4 + 64 + SIDECAR_EXT.len());
    name.push_str("idx-");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    name.push_str(SIDECAR_EXT);
    name
}

fn open_private_readonly_nofollow(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        Ok(File::from(fd))
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let meta = file.metadata()?;
        if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sidecar must not be a reparse point",
            ));
        }
        if !verify_windows_owner_only_dacl(file.as_raw_handle()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "sidecar file is not restricted to owner-only ACL",
            ));
        }
        Ok(file)
    }

    #[cfg(not(any(unix, windows)))]
    {
        File::open(path)
    }
}

fn create_private_exclusive_file(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::open(
            path,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let file = File::from(fd);
        let _ = rustix::fs::fchmod(&file, Mode::from_raw_mode(0o600));
        #[cfg(target_os = "macos")]
        crate::storage::set_macos_acl(&file, None)?;
        Ok(file)
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        };
        const WRITE_DAC: u32 = 0x0004_0000;
        let file = fs::OpenOptions::new()
            .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | WRITE_DAC)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        if let Err(err) = set_windows_owner_only_dacl(file.as_raw_handle()) {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(err);
        }
        Ok(file)
    }

    #[cfg(not(any(unix, windows)))]
    {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    }
}

#[cfg(windows)]
fn set_windows_owner_only_dacl(handle: *mut std::ffi::c_void) -> io::Result<()> {
    use std::ffi::c_void;

    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER_CLASS: u32 = 1;
    const SDDL_REVISION_1: u32 = 1;
    const SE_FILE_OBJECT: u32 = 1;
    const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;

    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        attributes: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn LocalFree(hmem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(
            process_handle: *mut c_void,
            desired_access: u32,
            token_handle: *mut *mut c_void,
        ) -> i32;
        fn GetTokenInformation(
            token_handle: *mut c_void,
            token_information_class: u32,
            token_information: *mut c_void,
            token_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            string_security_descriptor: *const u16,
            string_sd_revision: u32,
            security_descriptor: *mut *mut c_void,
            security_descriptor_size: *mut u32,
        ) -> i32;
        fn GetSecurityDescriptorDacl(
            security_descriptor: *mut c_void,
            dacl_present: *mut i32,
            dacl: *mut *mut c_void,
            dacl_defaulted: *mut i32,
        ) -> i32;
        fn SetSecurityInfo(
            handle: *mut c_void,
            object_type: u32,
            security_info: u32,
            owner: *mut c_void,
            group: *mut c_void,
            dacl: *mut c_void,
            sacl: *mut c_void,
        ) -> u32;
    }

    unsafe {
        let mut token: *mut c_void = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut needed: u32 = 0;
        let _ = GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
        if needed == 0 {
            let err = io::Error::last_os_error();
            CloseHandle(token);
            return Err(err);
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid_ptr = (*buf.as_ptr().cast::<SidAndAttributes>()).sid;
        let mut sid_wstr: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid_ptr, &mut sid_wstr) == 0 || sid_wstr.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut sid_len = 0usize;
        while *sid_wstr.add(sid_len) != 0 {
            sid_len += 1;
        }
        let sid_str = String::from_utf16_lossy(std::slice::from_raw_parts(sid_wstr, sid_len));
        LocalFree(sid_wstr.cast());

        let sddl = format!("D:P(A;OICI;FA;;;{sid_str})(A;OICI;FA;;;SY)\0");
        let sddl_wide: Vec<u16> = sddl.encode_utf16().collect();
        let mut sd: *mut c_void = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
            || sd.is_null()
        {
            return Err(io::Error::last_os_error());
        }
        let mut dacl_present: i32 = 0;
        let mut dacl_defaulted: i32 = 0;
        let mut dacl: *mut c_void = std::ptr::null_mut();
        let dacl_ok =
            GetSecurityDescriptorDacl(sd, &mut dacl_present, &mut dacl, &mut dacl_defaulted);
        let status = if dacl_ok != 0 && dacl_present != 0 {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        } else {
            1
        };
        LocalFree(sd);
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn verify_windows_owner_only_dacl(handle: *mut std::ffi::c_void) -> bool {
    use std::ffi::c_void;

    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER_CLASS: u32 = 1;
    const SE_FILE_OBJECT: u32 = 1;
    const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
    const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
    const ACL_SIZE_INFORMATION_CLASS: u32 = 2;
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;

    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        attributes: u32,
    }

    #[repr(C)]
    struct AclSizeInformation {
        ace_count: u32,
        acl_bytes_in_use: u32,
        acl_bytes_free: u32,
    }

    #[repr(C)]
    struct AceHeader {
        ace_type: u8,
        ace_flags: u8,
        ace_size: u16,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn LocalFree(hmem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(
            process_handle: *mut c_void,
            desired_access: u32,
            token_handle: *mut *mut c_void,
        ) -> i32;
        fn GetTokenInformation(
            token_handle: *mut c_void,
            token_information_class: u32,
            token_information: *mut c_void,
            token_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
        fn ConvertStringSidToSidW(string_sid: *const u16, sid: *mut *mut c_void) -> i32;
        fn EqualSid(sid1: *mut c_void, sid2: *mut c_void) -> i32;
        fn GetSecurityInfo(
            handle: *mut c_void,
            object_type: u32,
            security_info: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            security_descriptor: *mut *mut c_void,
        ) -> u32;
        fn GetAclInformation(
            acl: *mut c_void,
            acl_information: *mut c_void,
            acl_information_length: u32,
            acl_information_class: u32,
        ) -> i32;
        fn GetAce(acl: *mut c_void, ace_index: u32, ace: *mut *mut c_void) -> i32;
    }

    unsafe {
        let mut token: *mut c_void = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut needed: u32 = 0;
        let _ = GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
        if needed == 0 {
            CloseHandle(token);
            return false;
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return false;
        }
        let user_sid = (*buf.as_ptr().cast::<SidAndAttributes>()).sid;

        let system_sid_wide: Vec<u16> = "S-1-5-18\0".encode_utf16().collect();
        let mut system_sid: *mut c_void = std::ptr::null_mut();
        if ConvertStringSidToSidW(system_sid_wide.as_ptr(), &mut system_sid) == 0
            || system_sid.is_null()
        {
            return false;
        }

        let mut owner_sid: *mut c_void = std::ptr::null_mut();
        let mut dacl: *mut c_void = std::ptr::null_mut();
        let mut sd: *mut c_void = std::ptr::null_mut();
        let status = GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner_sid,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        );
        if status != 0 || sd.is_null() {
            LocalFree(system_sid);
            return false;
        }

        let mut allowed =
            !owner_sid.is_null() && EqualSid(owner_sid, user_sid) != 0 && !dacl.is_null();
        if allowed {
            let mut size_info = AclSizeInformation {
                ace_count: 0,
                acl_bytes_in_use: 0,
                acl_bytes_free: 0,
            };
            if GetAclInformation(
                dacl,
                std::ptr::from_mut(&mut size_info).cast(),
                std::mem::size_of::<AclSizeInformation>() as u32,
                ACL_SIZE_INFORMATION_CLASS,
            ) == 0
            {
                allowed = false;
            } else {
                for i in 0..size_info.ace_count {
                    let mut ace_ptr: *mut c_void = std::ptr::null_mut();
                    if GetAce(dacl, i, &mut ace_ptr) == 0 || ace_ptr.is_null() {
                        allowed = false;
                        break;
                    }
                    let hdr = &*ace_ptr.cast::<AceHeader>();
                    match hdr.ace_type {
                        ACCESS_ALLOWED_ACE_TYPE => {
                            if (hdr.ace_size as usize) <= 8 {
                                allowed = false;
                                break;
                            }
                            let ace_sid = ace_ptr.cast::<u8>().add(8).cast::<c_void>();
                            if EqualSid(ace_sid, user_sid) == 0
                                && EqualSid(ace_sid, system_sid) == 0
                            {
                                allowed = false;
                                break;
                            }
                        }
                        ACCESS_DENIED_ACE_TYPE => {}
                        _ => {
                            allowed = false;
                            break;
                        }
                    }
                }
            }
        }

        LocalFree(sd);
        LocalFree(system_sid);
        allowed
    }
}

/// Attempts to load and fully validate a cached persistent sidecar structural index.
///
/// Any validation mismatch, corruption, unsupported filesystem, or I/O error returns `None`
/// so indexing falls back silently to scanning the source file.
pub(crate) fn try_load_sidecar(
    file: &File,
    path: &Path,
    file_size: u64,
    delimiter: u8,
    encoding: Encoding,
    index_config: IndexConfig,
    cache_config: &IndexCacheConfig,
) -> Option<StructuralIndex> {
    if !cache_config.enabled
        || cache_config.max_cache_bytes == 0
        || file_size < cache_config.min_file_bytes
    {
        return None;
    }
    if !is_eligible_local_filesystem(file) {
        return None;
    }

    let cache_dir = resolve_and_harden_cache_dir(cache_config)?;
    let key = load_or_create_cache_key(&cache_dir)?;
    let identity = ExtendedIdentity::from_file(file).ok()?;
    if identity.file_size != file_size {
        return None;
    }

    let dialect = ScannerDialect::rfc4180(delimiter);
    let sidecar_path = cache_dir.join(keyed_sidecar_filename(
        &key,
        path,
        &identity,
        encoding,
        dialect,
        index_config.checkpoint_every,
    ));

    let mut sidecar_file = open_private_readonly_nofollow(&sidecar_path).ok()?;
    let meta = sidecar_file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_SIDECAR_FILE_BYTES {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != rustix::process::getuid().as_raw() || (meta.mode() & 0o077) != 0 {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            crate::storage::set_macos_acl(&sidecar_file, None).ok()?;
        }
    }

    let mut raw = Vec::with_capacity(meta.len() as usize);
    io::Read::by_ref(&mut sidecar_file)
        .take(MAX_SIDECAR_FILE_BYTES + 1)
        .read_to_end(&mut raw)
        .ok()?;
    if (raw.len() as u64) > MAX_SIDECAR_FILE_BYTES {
        return None;
    }

    let decoded = decode_sidecar_bytes(&raw)?;
    if !verify_sidecar_mac(&key, &raw) {
        return None;
    }
    let (exp_delim, exp_quote, exp_esc_kind, exp_esc_byte) = dialect_to_tags(dialect);
    if decoded.encoding_tag != encoding_to_tag(encoding)
        || decoded.delimiter != exp_delim
        || decoded.quote != exp_quote
        || decoded.escape_kind != exp_esc_kind
        || decoded.escape_byte != exp_esc_byte
    {
        return None;
    }
    if !decoded.identity.matches(&identity) {
        return None;
    }

    let (head_hash, tail_hash) = compute_head_tail_hashes(file, file_size).ok()?;
    if decoded.head_sha256 != head_hash || decoded.tail_sha256 != tail_hash {
        return None;
    }

    if !spot_check_checkpoints(
        file,
        file_size,
        decoded.indexed_rows,
        delimiter,
        encoding,
        &decoded.checkpoints,
    ) {
        return None;
    }

    // Re-check identity after spot-checks to catch concurrent source modification.
    let after_identity = ExtendedIdentity::from_file(file).ok()?;
    if !after_identity.matches(&identity) {
        return None;
    }

    let index = StructuralIndex::from_validated_sidecar(
        index_config,
        decoded.checkpoints,
        decoded.checkpoint_every,
        decoded.checkpoint_byte_stride,
        decoded.byte_stride_checkpoints,
        decoded.indexed_rows,
        decoded.maximum_width as usize,
        decoded.indexed_bytes,
    )
    .ok()?;

    // Best-effort LRU touch directly on the already-validated file handle (no second path lookup).
    #[cfg(unix)]
    {
        let _ = sidecar_file.set_modified(SystemTime::now());
    }

    Some(index)
}

/// Persists a completed structural index to the sidecar cache using atomic temp-file
/// write + `fsync` + rename, coordinated across processes with pre-write budget reservation.
///
/// Failures are silently ignored so cache I/O problems never break indexing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn save_sidecar(
    file: &File,
    path: &Path,
    expected_stamp: &SourceStamp,
    file_size: u64,
    delimiter: u8,
    encoding: Encoding,
    requested_checkpoint_every: u64,
    index: &StructuralIndex,
    cache_config: &IndexCacheConfig,
) {
    let _ = try_save_sidecar(
        file,
        path,
        expected_stamp,
        file_size,
        delimiter,
        encoding,
        requested_checkpoint_every,
        index,
        cache_config,
    );
}

#[allow(clippy::too_many_arguments)]
fn try_save_sidecar(
    file: &File,
    path: &Path,
    expected_stamp: &SourceStamp,
    file_size: u64,
    delimiter: u8,
    encoding: Encoding,
    requested_checkpoint_every: u64,
    index: &StructuralIndex,
    cache_config: &IndexCacheConfig,
) -> Option<()> {
    if !cache_config.enabled
        || cache_config.max_cache_bytes == 0
        || file_size < cache_config.min_file_bytes
        || index.indexed_bytes() != file_size
    {
        return None;
    }
    if !is_eligible_local_filesystem(file) {
        return None;
    }

    let cache_dir = resolve_and_harden_cache_dir(cache_config)?;
    let key = load_or_create_cache_key(&cache_dir)?;
    let identity = ExtendedIdentity::from_file(file).ok()?;
    if identity.file_size != file_size {
        return None;
    }
    if !source_matches_stamp(file, path, expected_stamp).ok()? {
        return None;
    }

    let (head_hash, tail_hash) = compute_head_tail_hashes(file, file_size).ok()?;
    let last_record_hash = compute_last_record_hash(file, file_size, index.checkpoints()).ok()?;
    let dialect = ScannerDialect::rfc4180(delimiter);

    let mut encoded = encode_sidecar_bytes(
        &identity,
        encoding,
        dialect,
        &head_hash,
        &tail_hash,
        &last_record_hash,
        index,
    )?;
    seal_sidecar_mac(&key, &mut encoded);
    let encoded_len = encoded.len() as u64;
    if encoded_len > cache_config.max_cache_bytes {
        return None;
    }

    // Re-check file identity and original indexed SourceStamp right before publishing.
    let recheck = ExtendedIdentity::from_file(file).ok()?;
    if !recheck.matches(&identity) || !source_matches_stamp(file, path, expected_stamp).ok()? {
        return None;
    }

    let final_name = keyed_sidecar_filename(
        &key,
        path,
        &identity,
        encoding,
        dialect,
        requested_checkpoint_every,
    );
    let final_path = cache_dir.join(&final_name);

    // Coordinate across processes and reserve full staging capacity before creating a temp file.
    let _lock = acquire_cache_lock(&cache_dir)?;
    if !reserve_cache_budget(&cache_dir, cache_config.max_cache_bytes, encoded_len) {
        return None;
    }

    let mut tmp_path = None;
    let mut tmp_file = None;
    for _ in 0..8 {
        let nonce = NEXT_TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let candidate = cache_dir.join(format!(".tmp-{}-{nonce}{TEMP_EXT}", std::process::id()));
        match create_private_exclusive_file(&candidate) {
            Ok(file) => {
                tmp_path = Some(candidate);
                tmp_file = Some(file);
                break;
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    let tmp_path = tmp_path?;
    let mut tmp_file = tmp_file?;

    let write_res = tmp_file
        .write_all(&encoded)
        .and_then(|()| tmp_file.sync_all());
    drop(tmp_file);
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
        return None;
    }

    if fs::rename(&tmp_path, &final_path).is_err() {
        let _ = fs::remove_file(&tmp_path);
        return None;
    }

    enforce_cache_budget(&cache_dir, cache_config.max_cache_bytes, Some(&final_name));
    Some(())
}

/// Evicts oldest Quarry-owned `.qidx` entries so `needed_bytes` fits within `max_cache_bytes`
/// alongside any active `.qidx.tmp` staging writers. Returns `true` if space is reserved.
pub(crate) fn reserve_cache_budget(
    cache_dir: &Path,
    max_cache_bytes: u64,
    needed_bytes: u64,
) -> bool {
    if needed_bytes > max_cache_bytes {
        return false;
    }
    let budget_after_reserve = max_cache_bytes - needed_bytes;
    let remaining = enforce_cache_budget_inner(cache_dir, budget_after_reserve, None);
    remaining.saturating_add(needed_bytes) <= max_cache_bytes
}

/// Concurrent-process-safe LRU eviction across `cache_dir`.
///
/// Counts BOTH published Quarry `.qidx` files AND active Quarry `.qidx.tmp` temp writers against
/// `max_cache_bytes`. Unrelated user files are never touched. Stale temp writers older than
/// `STALE_TEMP_AGE` are removed.
pub(crate) fn enforce_cache_budget(
    cache_dir: &Path,
    max_cache_bytes: u64,
    keep_name: Option<&str>,
) {
    let _ = enforce_cache_budget_inner(cache_dir, max_cache_bytes, keep_name);
}

fn enforce_cache_budget_inner(
    cache_dir: &Path,
    max_cache_bytes: u64,
    keep_name: Option<&str>,
) -> u64 {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return u64::MAX;
    };
    let now = SystemTime::now();
    let mut published: Vec<(PathBuf, String, u64, SystemTime)> = Vec::new();
    let mut total_bytes: u64 = 0;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        let len = meta.len();
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);

        if is_quarry_temp_filename(&name) {
            let is_stale = now
                .duration_since(modified)
                .is_ok_and(|age| age >= STALE_TEMP_AGE);
            if is_stale {
                if fs::remove_file(&path).is_err() {
                    total_bytes = total_bytes.saturating_add(len);
                }
            } else {
                // Active concurrent temp writer: keep intact and count toward budget.
                total_bytes = total_bytes.saturating_add(len);
            }
        } else if is_quarry_sidecar_filename(&name) {
            total_bytes = total_bytes.saturating_add(len);
            published.push((path, name, len, modified));
        }
    }

    if total_bytes <= max_cache_bytes {
        return total_bytes;
    }

    // Oldest modified first (deterministic tie-break by filename).
    published.sort_by(|a, b| a.3.cmp(&b.3).then_with(|| a.1.cmp(&b.1)));

    for (path, name, len, _) in published {
        if total_bytes <= max_cache_bytes {
            break;
        }
        if keep_name == Some(name.as_str()) {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                total_bytes = total_bytes.saturating_sub(len);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                total_bytes = total_bytes.saturating_sub(len);
            }
            Err(_) => {}
        }
    }
    total_bytes
}

/// Pure-Rust CRC32C (Castagnoli, polynomial `0x82F63B78`).
pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    const POLY: u32 = 0x82F6_3B78;
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = i as u32;
            let mut j = 0;
            while j < 8 {
                if (crc & 1) != 0 {
                    crc = (crc >> 1) ^ POLY;
                } else {
                    crc >>= 1;
                }
                j += 1;
            }
            table[i] = crc;
            i += 1;
        }
        table
    };

    let mut crc = !0u32;
    for &byte in bytes {
        crc = (crc >> 8) ^ TABLE[((crc as u8) ^ byte) as usize];
    }
    !crc
}

/// Minimal pure-Rust FIPS 180-4 SHA-256 implementation (zero external crate dependencies).
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_len: u64,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    fn new() -> Self {
        Self {
            state: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            buffer: [0u8; 64],
            buffer_len: 0,
            total_len: 0,
        }
    }

    fn digest(data: &[u8]) -> [u8; 32] {
        let mut h = Self::new();
        h.update(data);
        h.finalize()
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buffer_len > 0 {
            let need = 64 - self.buffer_len;
            let take = need.min(data.len());
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&data[..take]);
            self.buffer_len += take;
            data = &data[take..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffer_len = 0;
            }
        }
        while data.len() >= 64 {
            let block: [u8; 64] = data[..64].try_into().unwrap();
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffer_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;
        if self.buffer_len > 56 {
            self.buffer[self.buffer_len..64].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer_len = 0;
        }
        self.buffer[self.buffer_len..56].fill(0);
        self.buffer[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);

        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (i, &wi) in w.iter().enumerate() {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions as FsOpenOptions};
    use std::time::{Duration, SystemTime};

    use super::{
        CACHE_KEY_FILENAME, IndexCacheConfig, SIDECAR_EXT, SIDECAR_HEADER_LEN, Sha256, TEMP_EXT,
        clear_index_cache, crc32c, decode_sidecar_bytes, enforce_cache_budget,
        is_allowed_linux_fs_magic, is_allowed_macos_fstype, reserve_cache_budget,
    };
    use crate::{IndexConfig, OpenOptions, Session};

    #[test]
    fn sha256_and_crc32c_match_standard_vectors() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);

        let empty = Sha256::digest(b"");
        let empty_hex: String = empty.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            empty_hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let abc = Sha256::digest(b"abc");
        let abc_hex: String = abc.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            abc_hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let multi = Sha256::digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        let multi_hex: String = multi.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            multi_hex,
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn filesystem_allowlists_disable_network_and_fat_exfat_volumes() {
        assert!(is_allowed_macos_fstype("apfs"));
        assert!(is_allowed_macos_fstype("hfs"));
        assert!(!is_allowed_macos_fstype("msdos"));
        assert!(!is_allowed_macos_fstype("exfat"));
        assert!(!is_allowed_macos_fstype("smbfs"));
        assert!(!is_allowed_macos_fstype("nfs"));
        assert!(!is_allowed_macos_fstype("macfuse"));

        assert!(is_allowed_linux_fs_magic(0xEF53)); // ext4
        assert!(is_allowed_linux_fs_magic(0x9123_683E)); // btrfs
        assert!(!is_allowed_linux_fs_magic(0x4d44)); // msdos/vfat
        assert!(!is_allowed_linux_fs_magic(0x2011_bab0)); // exfat
        assert!(!is_allowed_linux_fs_magic(0x6969)); // nfs
        assert!(!is_allowed_linux_fs_magic(0xfe53_4d42)); // smb2
        assert!(!is_allowed_linux_fs_magic(0x6573_5546)); // fuse
    }

    #[test]
    fn sidecar_roundtrip_hit_invalidation_privacy_and_clear_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("private-cache");
        let csv_path = dir.path().join("sensitive_payroll_2026_secret_name.csv");

        let mut payload = Vec::new();
        payload.extend_from_slice(b"secret_header_alpha,secret_header_beta\n");
        for i in 1..=2_500_u32 {
            payload.extend_from_slice(
                format!("SECRET_CELL_TOKEN_{i:05},\"value\nline_{i}\"\n").as_bytes(),
            );
        }
        fs::write(&csv_path, &payload).unwrap();

        let cache_cfg = IndexCacheConfig {
            enabled: true,
            cache_dir: Some(cache_dir.clone()),
            max_cache_bytes: 16 * 1024 * 1024,
            min_file_bytes: 0,
        };
        let index_cfg = IndexConfig {
            chunk_bytes: 16 * 1024,
            checkpoint_every: 256,
            memory_budget_bytes: 16 * 1024 * 1024,
        };

        // First open: cache miss, writes sidecar.
        let session = Session::open(&csv_path, OpenOptions::default()).unwrap();
        let miss_index = session
            .start_indexing_with_options(index_cfg, Some(2), cache_cfg.clone())
            .unwrap()
            .wait()
            .unwrap();
        assert!(!miss_index.telemetry().cache_hit);
        assert_eq!(miss_index.indexed_rows(), 2_501);

        // Verify cache directory permissions (0700), file permissions (0600),
        // opaque keyed filename, and zero leaked headers/cells/filenames.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::symlink_metadata(&cache_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
        let mut qidx_files = Vec::new();
        for entry in fs::read_dir(&cache_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.contains("sensitive") && !name.contains("payroll"),
                "sidecar filename leaked source name: {name}"
            );
            if name.ends_with(SIDECAR_EXT) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let file_mode = entry.metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(file_mode, 0o600);
                }
                let raw = fs::read(entry.path()).unwrap();
                assert!(!raw.windows(12).any(|w| w == b"secret_heade"));
                assert!(!raw.windows(12).any(|w| w == b"SECRET_CELL_"));
                assert!(!raw.windows(9).any(|w| w == b"sensitive"));
                qidx_files.push(entry.path());
            }
        }
        assert_eq!(qidx_files.len(), 1);

        // Second open: validated cache hit, exact structural match + working row seek!
        let hit_index = session
            .start_indexing_with_options(index_cfg, Some(2), cache_cfg.clone())
            .unwrap()
            .wait()
            .unwrap();
        assert!(hit_index.telemetry().cache_hit);
        assert_eq!(hit_index, miss_index);
        let rows = session.read_rows(&hit_index, 1_500, 5).unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].fields[0], b"SECRET_CELL_TOKEN_01500");

        // Smaller memory budget compacts checkpoints cleanly on load.
        let tiny_budget = IndexConfig {
            chunk_bytes: 16 * 1024,
            checkpoint_every: 256,
            memory_budget_bytes: 64, // 4 checkpoints
        };
        let compacted = session
            .start_indexing_with_options(tiny_budget, Some(2), cache_cfg.clone())
            .unwrap()
            .wait()
            .unwrap();
        assert!(compacted.telemetry().cache_hit);
        assert!(compacted.memory_bytes() <= 64);
        let row_after_compact = session.read_rows(&compacted, 2_000, 1).unwrap();
        assert_eq!(row_after_compact[0].fields[0], b"SECRET_CELL_TOKEN_02000");

        // Corrupting the sidecar falls back silently to normal indexing (zero panic/error).
        let mut corrupted = fs::read(&qidx_files[0]).unwrap();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xFF;
        fs::write(&qidx_files[0], &corrupted).unwrap();
        let recovered = session
            .start_indexing_with_options(index_cfg, Some(2), cache_cfg.clone())
            .unwrap()
            .wait()
            .unwrap();
        assert!(!recovered.telemetry().cache_hit);
        assert_eq!(recovered, miss_index);

        // Modifying source bytes invalidates sidecar even if size is identical.
        let mut modified = payload.clone();
        modified[100] = b'Z';
        fs::write(&csv_path, &modified).unwrap();
        let session2 = Session::open(&csv_path, OpenOptions::default()).unwrap();
        let post_edit = session2
            .start_indexing_with_options(index_cfg, Some(2), cache_cfg.clone())
            .unwrap()
            .wait()
            .unwrap();
        assert!(!post_edit.telemetry().cache_hit);

        // Clear cache removes every sidecar and key file.
        let cleared = clear_index_cache(Some(&cache_dir)).unwrap();
        assert!(cleared.files_removed >= 2);
        assert!(cleared.bytes_reclaimed > 0);
        assert!(!cache_dir.join(CACHE_KEY_FILENAME).exists());
    }

    #[test]
    fn lru_eviction_counts_active_temp_writers_and_evicts_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();

        let oldest_name = format!("idx-{}{SIDECAR_EXT}", "a".repeat(64));
        let middle_name = format!("idx-{}{SIDECAR_EXT}", "b".repeat(64));
        let newest_name = format!("idx-{}{SIDECAR_EXT}", "c".repeat(64));
        let oldest = cache_dir.join(&oldest_name);
        let middle = cache_dir.join(&middle_name);
        let newest = cache_dir.join(&newest_name);
        let active_tmp = cache_dir.join(format!(".tmp-999-1{TEMP_EXT}"));
        let unrelated = cache_dir.join("analysis.qidx");

        fs::write(&oldest, vec![1u8; 400]).unwrap();
        fs::write(&middle, vec![2u8; 400]).unwrap();
        fs::write(&newest, vec![3u8; 400]).unwrap();
        fs::write(&active_tmp, vec![4u8; 400]).unwrap();
        fs::write(&unrelated, vec![5u8; 400]).unwrap();

        let now = SystemTime::now();
        FsOpenOptions::new()
            .write(true)
            .open(&oldest)
            .unwrap()
            .set_modified(now - Duration::from_secs(300))
            .unwrap();
        FsOpenOptions::new()
            .write(true)
            .open(&middle)
            .unwrap()
            .set_modified(now - Duration::from_secs(200))
            .unwrap();
        FsOpenOptions::new()
            .write(true)
            .open(&newest)
            .unwrap()
            .set_modified(now - Duration::from_secs(100))
            .unwrap();

        // Total managed is 1600 bytes including active_tmp (400B). Cap at 900 bytes -> must keep
        // active_tmp (400B) + newest (400B) = 800B, evicting oldest and middle, while leaving
        // unrelated `analysis.qidx` untouched!
        enforce_cache_budget(cache_dir, 900, Some(&newest_name));

        assert!(!oldest.exists(), "oldest sidecar should be evicted");
        assert!(
            !middle.exists(),
            "middle sidecar should be evicted due to active temp writer budget"
        );
        assert!(newest.exists(), "newest sidecar should remain");
        assert!(
            active_tmp.exists(),
            "active temp writer must not be deleted"
        );
        assert!(
            unrelated.exists(),
            "unrelated .qidx file must never be evicted"
        );

        // Pre-write reservation refuses to exceed cap when active temp writer leaves insufficient room.
        assert!(
            !reserve_cache_budget(cache_dir, 700, 400),
            "400B active temp + 400B needed exceeds 700B cap even after evicting newest"
        );
        assert!(
            !newest.exists(),
            "reservation attempted eviction of evictable entries before declining"
        );
        assert!(unrelated.exists(), "unrelated file still untouched");
    }

    #[test]
    fn untrusted_decoder_fuzz_never_panics_and_rejects_malformed_inputs() {
        // Truncated, empty, wrong magic, wrong version, huge checkpoint_count pre-alloc,
        // non-monotonic offsets, CRC mismatches, random deterministic mutations.
        assert!(decode_sidecar_bytes(b"").is_none());
        assert!(decode_sidecar_bytes(b"QIDX").is_none());
        assert!(decode_sidecar_bytes(&vec![0u8; SIDECAR_HEADER_LEN]).is_none());

        // Craft a header with valid CRC32C but huge checkpoint_count (100_000_000) to verify
        // capped pre-allocation rejects without OOM or panic.
        let mut crafted = vec![0u8; SIDECAR_HEADER_LEN + 16];
        crafted[0..4].copy_from_slice(b"QIDX");
        crafted[4..8].copy_from_slice(&1u32.to_le_bytes());
        crafted[224..232].copy_from_slice(&1u64.to_le_bytes());
        crafted[240..244].copy_from_slice(&100_000_000u32.to_le_bytes());
        let crc = crc32c(&crafted[16..]);
        crafted[12..16].copy_from_slice(&crc.to_le_bytes());
        assert!(decode_sidecar_bytes(&crafted).is_none());

        // Craft a header with valid CRC32C and non-monotonic checkpoint offsets.
        let mut non_mono = vec![0u8; SIDECAR_HEADER_LEN + 32];
        non_mono[0..4].copy_from_slice(b"QIDX");
        non_mono[4..8].copy_from_slice(&1u32.to_le_bytes());
        non_mono[24..32].copy_from_slice(&1000u64.to_le_bytes()); // file_size
        non_mono[200..208].copy_from_slice(&10u64.to_le_bytes()); // indexed_rows
        non_mono[208..216].copy_from_slice(&1000u64.to_le_bytes()); // indexed_bytes
        non_mono[216..224].copy_from_slice(&3u64.to_le_bytes()); // max_width
        non_mono[224..232].copy_from_slice(&1u64.to_le_bytes()); // checkpoint_every
        non_mono[240..244].copy_from_slice(&2u32.to_le_bytes()); // checkpoint_count
        // cp[0] = (0, 0), cp[1] = (5, 0) -> non-monotonic offset!
        non_mono[SIDECAR_HEADER_LEN + 16..SIDECAR_HEADER_LEN + 24]
            .copy_from_slice(&5u64.to_le_bytes());
        let crc = crc32c(&non_mono[16..]);
        non_mono[12..16].copy_from_slice(&crc.to_le_bytes());
        assert!(decode_sidecar_bytes(&non_mono).is_none());

        // Deterministic PRNG fuzz across 500 random inputs & length mutations.
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        for case in 0..500_usize {
            let len = (case * 37) % (SIDECAR_HEADER_LEN + 128);
            let mut buf = vec![0u8; len];
            for b in &mut buf {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                *b = (seed >> 56) as u8;
            }
            if buf.len() >= 16 && case % 3 == 0 {
                buf[0..4].copy_from_slice(b"QIDX");
                buf[4..8].copy_from_slice(&1u32.to_le_bytes());
                let crc = crc32c(&buf[16..]);
                buf[12..16].copy_from_slice(&crc.to_le_bytes());
            }
            let _ = decode_sidecar_bytes(&buf);
        }
    }

    #[test]
    fn sidecar_accepts_eof_checkpoint_without_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let csv_path = dir.path().join("no_trailing_lf.csv");
        // Four records with no trailing newline and `checkpoint_every: 2` emits a final
        // checkpoint at `(row: 4, offset: file_size)`.
        fs::write(&csv_path, b"a,b\n1,2\n3,4\n5,6").unwrap();

        let cache_cfg = IndexCacheConfig {
            enabled: true,
            cache_dir: Some(cache_dir),
            max_cache_bytes: 16 * 1024 * 1024,
            min_file_bytes: 0,
        };
        let index_cfg = IndexConfig {
            chunk_bytes: 64,
            checkpoint_every: 2,
            memory_budget_bytes: 64 * 1024,
        };

        let session = Session::open(&csv_path, OpenOptions::default()).unwrap();
        let first = session
            .start_indexing_with_options(index_cfg, Some(1), cache_cfg.clone())
            .unwrap()
            .wait()
            .unwrap();
        assert!(!first.telemetry().cache_hit);
        assert_eq!(first.indexed_rows(), 4);
        assert_eq!(
            first.checkpoints().last().unwrap().offset,
            session.file_size
        );

        let second = session
            .start_indexing_with_options(index_cfg, Some(1), cache_cfg)
            .unwrap()
            .wait()
            .unwrap();
        assert!(
            second.telemetry().cache_hit,
            "unchanged file without trailing newline should hit sidecar cache"
        );
        assert_eq!(second, first);
    }

    #[test]
    fn zero_max_cache_bytes_disables_saving_and_loading() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("zero_cache");
        let csv_path = dir.path().join("sample.csv");
        fs::write(&csv_path, b"a,b\n1,2\n3,4\n").unwrap();

        let cache_cfg = IndexCacheConfig {
            enabled: true,
            cache_dir: Some(cache_dir.clone()),
            max_cache_bytes: 0,
            min_file_bytes: 0,
        };
        let index_cfg = IndexConfig {
            chunk_bytes: 64,
            checkpoint_every: 2,
            memory_budget_bytes: 64 * 1024,
        };

        let session = Session::open(&csv_path, OpenOptions::default()).unwrap();
        let first = session
            .start_indexing_with_options(index_cfg, Some(1), cache_cfg.clone())
            .unwrap()
            .wait()
            .unwrap();
        assert!(!first.telemetry().cache_hit);
        assert!(
            !cache_dir.exists(),
            "max_cache_bytes == 0 must not create or populate the cache directory"
        );

        let second = session
            .start_indexing_with_options(index_cfg, Some(1), cache_cfg)
            .unwrap()
            .wait()
            .unwrap();
        assert!(!second.telemetry().cache_hit);
    }

    #[test]
    fn forged_sidecar_with_valid_crc_but_wrong_mac_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("mac_cache");
        let csv_path = dir.path().join("sample.csv");
        fs::write(&csv_path, b"a,b\n1,2\n3,4\n5,6\n").unwrap();

        let cache_cfg = IndexCacheConfig {
            enabled: true,
            cache_dir: Some(cache_dir.clone()),
            max_cache_bytes: 16 * 1024 * 1024,
            min_file_bytes: 0,
        };
        let index_cfg = IndexConfig {
            chunk_bytes: 64,
            checkpoint_every: 2,
            memory_budget_bytes: 64 * 1024,
        };

        let session = Session::open(&csv_path, OpenOptions::default()).unwrap();
        let first = session
            .start_indexing_with_options(index_cfg, Some(1), cache_cfg.clone())
            .unwrap()
            .wait()
            .unwrap();
        assert!(!first.telemetry().cache_hit);

        // Tamper with `max_width` inside the sidecar and recompute the unkeyed CRC32C
        // without updating the keyed MAC.
        let sidecar_path = fs::read_dir(&cache_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|ext| ext == "qidx"))
            .expect("sidecar file should exist");
        let mut raw = fs::read(&sidecar_path).unwrap();
        raw[216..224].copy_from_slice(&999u64.to_le_bytes());
        let crc = crc32c(&raw[16..]);
        raw[12..16].copy_from_slice(&crc.to_le_bytes());
        fs::write(&sidecar_path, &raw).unwrap();
        assert!(
            decode_sidecar_bytes(&raw).is_some(),
            "forged buffer has a valid structural CRC32C"
        );

        let second = session
            .start_indexing_with_options(index_cfg, Some(1), cache_cfg)
            .unwrap()
            .wait()
            .unwrap();
        assert!(
            !second.telemetry().cache_hit,
            "forged sidecar with invalid keyed MAC must be rejected"
        );
        assert_eq!(second.maximum_width(), first.maximum_width());
    }

    #[test]
    fn checkpoint_policy_separation_partial_key_recovery_and_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("policy_cache");
        let csv_path = dir.path().join("sample.csv");
        let mut payload = Vec::new();
        payload.extend_from_slice(b"a,b\n");
        for i in 0..200_u32 {
            payload.extend_from_slice(format!("{i},val\n").as_bytes());
        }
        fs::write(&csv_path, &payload).unwrap();

        let cache_cfg = IndexCacheConfig {
            enabled: true,
            cache_dir: Some(cache_dir.clone()),
            max_cache_bytes: 16 * 1024 * 1024,
            min_file_bytes: 0,
        };

        // First open with sparse policy `checkpoint_every: u64::MAX`.
        let session = Session::open(&csv_path, OpenOptions::default()).unwrap();
        let sparse = session
            .start_indexing_with_options(
                IndexConfig {
                    chunk_bytes: 1024,
                    checkpoint_every: u64::MAX,
                    memory_budget_bytes: 64 * 1024,
                },
                Some(1),
                cache_cfg.clone(),
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(!sparse.telemetry().cache_hit);
        assert_eq!(sparse.checkpoints().len(), 1);

        // Next open with dense policy `checkpoint_every: 16` must NOT reuse the sparse sidecar.
        let dense = session
            .start_indexing_with_options(
                IndexConfig {
                    chunk_bytes: 1024,
                    checkpoint_every: 16,
                    memory_budget_bytes: 64 * 1024,
                },
                Some(1),
                cache_cfg.clone(),
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(!dense.telemetry().cache_hit);
        assert!(dense.checkpoints().len() > 1);

        // Unrelated `.qidx` file in the cache directory is preserved by `clear_index_cache`.
        let unrelated = cache_dir.join("analysis.qidx");
        fs::write(&unrelated, b"user data").unwrap();
        let cleared = clear_index_cache(Some(&cache_dir)).unwrap();
        assert!(cleared.files_removed >= 3);
        assert!(unrelated.exists(), "unrelated .qidx must not be deleted");

        // Recover cleanly from a truncated partial `.cache-key` left by a crashed process.
        fs::write(cache_dir.join(CACHE_KEY_FILENAME), b"partial").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                cache_dir.join(CACHE_KEY_FILENAME),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let recovered = session
            .start_indexing_with_options(
                IndexConfig {
                    chunk_bytes: 1024,
                    checkpoint_every: 16,
                    memory_budget_bytes: 64 * 1024,
                },
                Some(1),
                cache_cfg.clone(),
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(!recovered.telemetry().cache_hit);
        assert_eq!(
            fs::metadata(cache_dir.join(CACHE_KEY_FILENAME))
                .unwrap()
                .len(),
            32
        );
        let hit = session
            .start_indexing_with_options(
                IndexConfig {
                    chunk_bytes: 1024,
                    checkpoint_every: 16,
                    memory_budget_bytes: 64 * 1024,
                },
                Some(1),
                cache_cfg,
            )
            .unwrap()
            .wait()
            .unwrap();
        assert!(hit.telemetry().cache_hit);
    }

    #[cfg(unix)]
    #[test]
    fn existing_shared_user_cache_dir_is_rejected_without_changing_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared_dir");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();

        let csv_path = dir.path().join("sample.csv");
        fs::write(&csv_path, b"a,b\n1,2\n").unwrap();

        let cache_cfg = IndexCacheConfig {
            enabled: true,
            cache_dir: Some(shared.clone()),
            max_cache_bytes: 16 * 1024 * 1024,
            min_file_bytes: 0,
        };
        let session = Session::open(&csv_path, OpenOptions::default()).unwrap();
        let idx = session
            .start_indexing_with_options(IndexConfig::default(), Some(1), cache_cfg)
            .unwrap()
            .wait()
            .unwrap();
        assert!(!idx.telemetry().cache_hit);
        let mode = fs::symlink_metadata(&shared).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "existing user-selected shared directory permissions must not be modified"
        );
        assert_eq!(
            fs::read_dir(&shared).unwrap().count(),
            0,
            "no cache files should be written into an existing shared directory"
        );

        // Symlink ancestor must be rejected before creating or clearing.
        let link = dir.path().join("link_parent");
        symlink(&shared, &link).unwrap();
        let nested = link.join("child");
        assert!(clear_index_cache(Some(&nested)).is_err());
    }
}
