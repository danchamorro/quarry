//! Read-only discovery of the widest logical record in a session.
use crate::encoding::EncodedScanner;
use crate::export::{open_unchanged_source, source_matches_stamp};
use crate::{DEFAULT_READ_CHUNK, QuarryError, Session};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

#[derive(Default)]
struct Progress {
    bytes: AtomicU64,
    cancelled: AtomicBool,
}

pub struct ColumnDiscoveryJob {
    progress: Arc<Progress>,
    worker: Option<JoinHandle<Result<Option<usize>, QuarryError>>>,
}

impl Session {
    pub fn start_column_discovery(&self) -> Result<ColumnDiscoveryJob, QuarryError> {
        let path = self.path.clone();
        let stamp = self.source_stamp.clone();
        let encoding = self.encoding;
        let delimiter = self.dialect.delimiter;
        let progress = Arc::new(Progress::default());
        let state = Arc::clone(&progress);
        let worker = thread::Builder::new()
            .name("quarry-find-columns".into())
            .spawn(move || {
                let mut file = open_unchanged_source(&path, &stamp)?;
                let mut scanner = EncodedScanner::<true>::new(delimiter, encoding)?;
                let mut chunk = vec![0; encoding.chunk_size(DEFAULT_READ_CHUNK)];
                let mut offset = 0;
                while !state.cancelled.load(Ordering::Acquire) {
                    let read = encoding.read_chunk(&mut file, &mut chunk)?;
                    if read == 0 {
                        scanner.finish(offset, |_| {})?;
                        if offset != stamp.file_size()
                            || !source_matches_stamp(&file, &path, &stamp)?
                        {
                            return Err(QuarryError::SourceChanged);
                        }
                        return Ok(Some(scanner.maximum_width()));
                    }
                    if offset + read as u64 > stamp.file_size() {
                        return Err(QuarryError::SourceChanged);
                    }
                    scanner.scan_chunk(&chunk[..read], offset, |_| {})?;
                    offset += read as u64;
                    state.bytes.store(offset, Ordering::Release);
                }
                Ok(None)
            })?;
        Ok(ColumnDiscoveryJob {
            progress,
            worker: Some(worker),
        })
    }
}

impl ColumnDiscoveryJob {
    pub fn bytes_scanned(&self) -> u64 {
        self.progress.bytes.load(Ordering::Acquire)
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn cancel(&self) {
        self.progress.cancelled.store(true, Ordering::Release);
    }

    pub fn wait(mut self) -> Result<Option<usize>, QuarryError> {
        let result = self
            .worker
            .take()
            .expect("discovery worker is present")
            .join()
            .map_err(|_| QuarryError::WorkerPanicked)?;
        if self.progress.cancelled.load(Ordering::Acquire) {
            Ok(None)
        } else {
            result
        }
    }
}

impl Drop for ColumnDiscoveryJob {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.cancel();
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HeaderMode, OpenOptions};
    use std::fs;
    use std::path::Path;

    fn open(path: &Path, bytes: &[u8]) -> Session {
        fs::write(path, bytes).unwrap();
        Session::open(
            path,
            OpenOptions {
                delimiter: Some(b';'),
                header_mode: HeaderMode::NoHeader,
                rows: 1,
                sample_bytes: 32,
                bootstrap_limit: 32,
            },
        )
        .unwrap()
    }

    #[test]
    fn discovers_logical_fields_across_chunks_and_encodings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ragged.csv");
        let csv = format!(
            "1;2\n\"{};x\n\"\"quoted\"\"\";;end;\r\nshort",
            "a".repeat(DEFAULT_READ_CHUNK)
        );
        for bytes in [
            csv.as_bytes().to_vec(),
            [b"\xef\xbb\xbf".as_slice(), csv.as_bytes()].concat(),
            [
                vec![0xff, 0xfe],
                csv.encode_utf16().flat_map(u16::to_le_bytes).collect(),
            ]
            .concat(),
            [
                vec![0xfe, 0xff],
                csv.encode_utf16().flat_map(u16::to_be_bytes).collect(),
            ]
            .concat(),
        ] {
            let session = open(&path, &bytes);
            assert_eq!(
                session.start_column_discovery().unwrap().wait().unwrap(),
                Some(4)
            );
            let index = session
                .start_indexing(crate::IndexConfig::default())
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(index.maximum_width(), 4);
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        for (csv, width) in [("", 0), ("\n", 1), ("\u{feff}", 0), ("a\n;\"x;y\";;", 4)] {
            let session = open(&path, csv.as_bytes());
            assert_eq!(
                session.start_column_discovery().unwrap().wait().unwrap(),
                Some(width)
            );
            let index = session
                .start_indexing(crate::IndexConfig::default())
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(index.maximum_width(), width);
        }
    }

    #[test]
    fn discovery_and_adoption_report_a_vanished_or_nonregular_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.csv");
        let session = open(&path, b"a;b");
        fs::remove_file(&path).unwrap();
        for missing in [true, false] {
            if !missing {
                fs::create_dir(&path).unwrap();
            }
            assert!(matches!(
                session.start_column_discovery().unwrap().wait(),
                Err(QuarryError::SourceChanged)
            ));
            assert!(matches!(
                session.ensure_source_unchanged(),
                Err(QuarryError::SourceChanged)
            ));
        }
    }

    #[test]
    fn malformed_input_does_not_return_a_partial_width() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late-error.csv");
        for tail in ["bad\"quote", "\"unfinished", "\"quoted\"x", "\"closed\"\r"] {
            let csv = format!("{}{tail}", "1;2;3\n".repeat(20));
            let session = open(&path, csv.as_bytes());
            assert!(session.start_column_discovery().unwrap().wait().is_err());
            assert_eq!(fs::read(&path).unwrap(), csv.as_bytes());
        }
    }
}
