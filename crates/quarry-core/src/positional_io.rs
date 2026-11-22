//! Positional file I/O (`pread` / `seek_read`) and OS read-ahead hints.
//!
//! Quarry deliberately avoids memory-mapping (`mmap`) user source files so external
//! truncation or network-volume errors surface as typed I/O or `SourceChanged` errors
//! instead of unrecoverable `SIGBUS` faults (ADR 0001 amendment).

use std::fs::File;
use std::io;

/// Read up to `buf.len()` bytes starting at `offset` without moving the file cursor.
///
/// Loops across `EINTR` and short reads until either `buf` is filled or EOF is reached,
/// returning the number of bytes read.
pub(crate) fn read_at_exact_or_eof(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let pos = offset
            .checked_add(filled as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file offset overflow"))?;
        match read_at_once(file, pos, &mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[cfg(unix)]
fn read_at_once(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
fn read_at_once(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

/// Issue a best-effort asynchronous read-ahead hint for `[offset, offset + len)`.
///
/// Errors are intentionally ignored because not all filesystems or OSes support hints.
pub(crate) fn advise_readahead(file: &File, offset: u64, len: u64) {
    if len == 0 {
        return;
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        if let Ok(ra_offset) = i64::try_from(offset) {
            let ra_count = len.min(libc::c_int::MAX as u64) as libc::c_int;
            let advisory = libc::radvisory {
                ra_offset,
                ra_count,
            };
            // SAFETY: `file` is a valid open file descriptor and `advisory` is a valid
            // initialized `libc::radvisory` struct passed to `F_RDADVISE`.
            let _ = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_RDADVISE, &advisory) };
        }
    }

    #[cfg(target_os = "linux")]
    {
        let _ = rustix::fs::fadvise(
            file,
            offset,
            std::num::NonZeroU64::new(len),
            rustix::fs::Advice::Sequential,
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (file, offset, len);
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::sync::Arc;
    use std::thread;

    use super::{advise_readahead, read_at_exact_or_eof};

    #[test]
    fn concurrent_positional_reads_return_exact_ranges_without_cursor_interference() {
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        let data: Vec<u8> = (0..262_144_u32).map(|i| (i % 251) as u8).collect();
        temp.write_all(&data).unwrap();
        temp.flush().unwrap();

        let file = Arc::new(File::open(temp.path()).unwrap());
        advise_readahead(&file, 0, data.len() as u64);

        let handles: Vec<_> = (0..8usize)
            .map(|worker| {
                let file = Arc::clone(&file);
                let expected = data.clone();
                thread::spawn(move || {
                    for stride in 0..32usize {
                        let start = ((worker * 37 + stride * 193) * 17) % expected.len();
                        let len = ((stride * 97 + 123) % 4096) + 1;
                        let mut buf = vec![0u8; len];
                        let read = read_at_exact_or_eof(&file, start as u64, &mut buf).unwrap();
                        let expected_len = len.min(expected.len() - start);
                        assert_eq!(read, expected_len);
                        assert_eq!(&buf[..read], &expected[start..start + expected_len]);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn positional_read_past_eof_returns_zero() {
        let mut temp = tempfile::NamedTempFile::new().unwrap();
        temp.write_all(b"abc").unwrap();
        temp.flush().unwrap();
        let file = File::open(temp.path()).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(read_at_exact_or_eof(&file, 100, &mut buf).unwrap(), 0);
    }

    #[test]
    fn external_truncation_returns_eof_and_source_changed_without_sigbus() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncation.csv");
        let mut payload = Vec::with_capacity(131_072);
        payload.extend_from_slice(b"id,value\n");
        for i in 1..=8_000_u32 {
            payload.extend_from_slice(format!("{i},row-{i:05}\n").as_bytes());
        }
        std::fs::write(&path, &payload).unwrap();

        let reader = File::open(&path).unwrap();
        let session = crate::Session::open(&path, crate::OpenOptions::default()).unwrap();
        let index = session
            .start_indexing_with_options(
                crate::IndexConfig::default(),
                Some(4),
                crate::IndexCacheConfig::disabled(),
            )
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(index.indexed_rows(), 8_001);

        // Externally truncate the live source file while handles and sessions remain open.
        let writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.set_len(16).unwrap();
        writer.sync_all().unwrap();

        // Reading past the new EOF returns 0 bytes cleanly (no SIGBUS).
        let mut buf = [0u8; 4096];
        assert_eq!(read_at_exact_or_eof(&reader, 65_536, &mut buf).unwrap(), 0);
        // Viewport reads past the truncated EOF return empty cleanly without crashing.
        assert!(session.read_rows(&index, 500, 10).unwrap().is_empty());

        // Parallel re-indexing and parallel search report typed `SourceChanged` errors cleanly.
        let reindex = session
            .start_indexing_with_options(
                crate::IndexConfig::default(),
                Some(4),
                crate::IndexCacheConfig::disabled(),
            )
            .and_then(|job| job.wait());
        assert!(matches!(reindex, Err(crate::QuarryError::SourceChanged)));

        let search = session
            .start_search_with_options(
                &index,
                b"row-07000".to_vec(),
                crate::SearchPosition { row: 0, column: 0 },
                std::collections::BTreeMap::new(),
                crate::CaseSensitivity::Sensitive,
                Some(4),
            )
            .and_then(|job| job.wait());
        assert!(matches!(search, Err(crate::QuarryError::SourceChanged)));
    }
}
