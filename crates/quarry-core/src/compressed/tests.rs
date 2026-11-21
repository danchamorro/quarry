use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::{DeflateEncoder, GzEncoder};

use super::test_pause::{Checkpoint, Pause};
use super::*;
use crate::{
    CaseSensitivity, CombineOptions, FilterExportOutcome, FilterOperator, FilterPredicate,
    FilterQuery, InspectionEdits, InspectionJob, InspectionOptions, InspectionStatus, JoinConfig,
    JoinInput, RecoveryJob, SaveAsOutcome, analyze_join, check_combine_files, read_join_header,
};

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn raw_deflate(data: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

#[derive(Default)]
struct Header<'a> {
    extra: Option<Vec<u8>>,
    name: Option<&'a [u8]>,
    comment: Option<&'a [u8]>,
    header_crc: bool,
    flags: u8,
    method: Option<u8>,
}

/// Build one gzip member by hand so every optional header field is exercised.
fn member(data: &[u8], header: Header<'_>) -> Vec<u8> {
    let mut flags = header.flags;
    flags |= if header.extra.is_some() {
        FLAG_EXTRA
    } else {
        0
    };
    flags |= if header.name.is_some() { FLAG_NAME } else { 0 };
    flags |= if header.comment.is_some() {
        FLAG_COMMENT
    } else {
        0
    };
    flags |= if header.header_crc { FLAG_HCRC } else { 0 };
    let mut bytes = vec![
        0x1f,
        0x8b,
        header.method.unwrap_or(8),
        flags,
        1,
        2,
        3,
        4,
        0,
        255,
    ];
    if let Some(extra) = &header.extra {
        bytes.extend((extra.len() as u16).to_le_bytes());
        bytes.extend(extra);
    }
    for text in [header.name, header.comment].into_iter().flatten() {
        bytes.extend(text);
        bytes.push(0);
    }
    if header.header_crc {
        let mut crc = Crc::new();
        crc.update(&bytes);
        bytes.extend((crc.sum() as u16).to_le_bytes());
    }
    bytes.extend(raw_deflate(data));
    let mut crc = Crc::new();
    crc.update(data);
    bytes.extend(crc.sum().to_le_bytes());
    bytes.extend((data.len() as u32).to_le_bytes());
    bytes
}

/// Decode through a tiny buffer and output window to exercise every boundary.
fn decode(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut reader = GzipMembers::new(BufReader::with_capacity(7, bytes));
    let mut output = Vec::new();
    let mut buffer = [0_u8; 5];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(output);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

fn is_corrupt(result: io::Result<Vec<u8>>) -> bool {
    match result {
        Ok(_) => false,
        Err(error) => error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<CorruptGzip>())
            .is_some(),
    }
}

fn expect_err<T>(result: Result<T, QuarryError>) -> QuarryError {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => error,
    }
}

fn reason(error: &QuarryError) -> &str {
    match error {
        QuarryError::CompressedFile { reason, .. } => reason,
        other => panic!("expected a compressed-file error, got {other:?}"),
    }
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect()
}

fn config(temp: &Path) -> CompressedOpenConfig {
    CompressedOpenConfig {
        min_free_space_reserve_bytes: 0,
        ..CompressedOpenConfig::new(temp)
    }
}

fn open_compressed(
    path: &Path,
    options: OpenOptions,
    encoding: Option<Encoding>,
    config: CompressedOpenConfig,
    cleanup: WorkingCopyCleanup,
) -> Result<(Session, StructuralIndex), QuarryError> {
    let bootstrap = CompressedBootstrap::open(path, options, encoding, None)?;
    match bootstrap.start_decompression(config, cleanup)?.wait()? {
        CompressedOpenOutcome::Complete { session, index } => Ok((session, index)),
        CompressedOpenOutcome::Cancelled => panic!("unexpected cancellation"),
    }
}

fn all_rows(session: &Session, index: &StructuralIndex) -> Vec<Row> {
    if index.indexed_rows() == 0 {
        return Vec::new();
    }
    session
        .read_rows(index, 0, index.indexed_rows() as usize)
        .unwrap()
}

fn plain(
    path: &Path,
    options: OpenOptions,
    encoding: Option<Encoding>,
) -> (Session, StructuralIndex) {
    let session = Session::open_with_encoding(path, options, encoding).unwrap();
    let index = session
        .start_indexing(IndexConfig::default())
        .unwrap()
        .wait()
        .unwrap();
    (session, index)
}

fn wait_for(mut ready: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

fn numbered_rows(count: usize) -> Vec<u8> {
    let mut data = b"id,value\n".to_vec();
    for row in 0..count {
        data.extend(format!("{row},v{row}\n").as_bytes());
    }
    data
}

// ---- Detection ---------------------------------------------------------------

#[test]
fn detects_gzip_and_names_every_unsupported_format() {
    let dir = tempfile::tempdir().unwrap();
    let gz = write(dir.path(), "sales.csv.gz", &gzip(b"a,b\n1,2\n"));
    assert_eq!(
        detect_compression(&gz).unwrap(),
        Some(CompressionFormat::Gzip)
    );
    // Content, not the name, identifies gzip.
    let disguised = write(dir.path(), "sales.csv", &gzip(b"a,b\n"));
    assert_eq!(
        detect_compression(&disguised).unwrap(),
        Some(CompressionFormat::Gzip)
    );
    let csv = write(dir.path(), "plain.csv", b"a,b\n1,2\n");
    assert_eq!(detect_compression(&csv).unwrap(), None);
    let empty = write(dir.path(), "empty.csv", b"");
    assert_eq!(detect_compression(&empty).unwrap(), None);
    // Text that merely begins like a bzip2 signature is still ordinary text.
    let bzh = write(dir.path(), "bzh.csv", b"BZh9,value\n1,2\n");
    assert_eq!(detect_compression(&bzh).unwrap(), None);

    let cases: [(&str, &[u8], &str); 12] = [
        ("a.zip", b"PK\x03\x04rest-of-zip", "ZIP"),
        ("a.csv", b"PK\x05\x06\0\0\0\0", "ZIP"),
        ("a.zst", b"\x28\xb5\x2f\xfd\0\0", "Zstandard"),
        ("a.dat", b"\x28\xb5\x2f\xfd\0\0", "Zstandard"),
        ("a.bz2", b"BZh91AY&SYrest", "bzip2"),
        ("a.csv", b"BZh91AY&SYrest", "bzip2"),
        ("a.xz", b"\xfd7zXZ\0rest", "XZ"),
        ("a.csv", b"\xfd7zXZ\0rest", "XZ"),
        ("plain.zst", b"a,b\n", "Zstandard"),
        ("plain.zip", b"a,b\n", "ZIP"),
        ("plain.bz2", b"a,b\n", "bzip2"),
        ("plain.xz", b"a,b\n", "XZ"),
    ];
    for (name, bytes, label) in cases {
        let path = write(dir.path(), name, bytes);
        let error = detect_compression(&path).unwrap_err();
        let reason = reason(&error);
        assert!(reason.contains(label), "{name}: {reason}");
        assert!(reason.contains("gzip (.gz)"), "{name}: {reason}");
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rejects_tar_archives_bad_methods_and_misnamed_gzip() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["bundle.tar.gz", "BUNDLE.TGZ"] {
        let path = write(dir.path(), name, &gzip(b"a,b\n"));
        assert!(reason(&detect_compression(&path).unwrap_err()).contains("tar archive"));
    }
    let tar_text = write(dir.path(), "named.tgz", b"a,b\n");
    assert!(reason(&detect_compression(&tar_text).unwrap_err()).contains("tar archive"));
    let method = write(dir.path(), "odd.gz", &[0x1f, 0x8b, 0x07, 0, 0, 0]);
    assert!(reason(&detect_compression(&method).unwrap_err()).contains("method 7"));
    let short = write(dir.path(), "short.gz", &[0x1f, 0x8b]);
    assert!(reason(&detect_compression(&short).unwrap_err()).contains("truncated"));
    let renamed = write(dir.path(), "notes.CSV.GZ", b"a,b\n1,2\n");
    assert!(reason(&detect_compression(&renamed).unwrap_err()).contains("gzip signature"));
}

#[test]
fn suggests_an_uncompressed_name_that_never_names_the_source() {
    let gzip = CompressionFormat::Gzip;
    let dir = Path::new("/data");
    let suggest = |name: &str| gzip.suggested_uncompressed_path(&dir.join(name));
    assert_eq!(suggest("sales.csv.gz"), dir.join("sales.csv"));
    assert_eq!(suggest("data.tsv.gz"), dir.join("data.tsv"));
    assert_eq!(suggest("Report.CSV.GZ"), dir.join("Report.CSV"));
    assert_eq!(suggest("data.gz"), dir.join("data.csv"));
    assert_eq!(suggest("data.csv"), dir.join("data-uncompressed.csv"));
    assert_eq!(suggest("archive"), dir.join("archive-uncompressed.csv"));
    assert_eq!(gzip.label(), "gzip");
    assert_eq!(gzip.extension(), "gz");
}

#[test]
fn ordinary_open_never_reads_compressed_bytes_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let gz = write(dir.path(), "sales.csv.gz", &gzip(b"a,b\n1,2\n"));
    let error = Session::open(&gz, OpenOptions::default()).unwrap_err();
    assert!(reason(&error).contains("gzip-compressed"));
    let zst = write(dir.path(), "sales.zst", b"\x28\xb5\x2f\xfd\0\0");
    assert!(
        reason(&Session::open(&zst, OpenOptions::default()).unwrap_err()).contains("Zstandard")
    );
    let csv = write(dir.path(), "plain.csv", b"a,b\n1,2\n");
    assert!(
        !Session::open(&csv, OpenOptions::default())
            .unwrap()
            .is_compressed()
    );
    let error = CompressedBootstrap::open(&csv, OpenOptions::default(), None, None).unwrap_err();
    assert!(reason(&error).contains("not compressed"));
}

// ---- Strict member reader -----------------------------------------------------

#[test]
fn reads_single_multi_member_and_optional_header_fields() {
    let data = b"name,value\n\"multi\nline\",2\n";
    assert_eq!(decode(&gzip(data)).unwrap(), data);
    assert_eq!(decode(&gzip(b"")).unwrap(), b"");

    let mut joined = gzip(b"first,");
    joined.extend(gzip(b""));
    joined.extend(gzip(b"half\nsecond,row\n"));
    let mut reader = GzipMembers::new(BufReader::new(joined.as_slice()));
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output, b"first,half\nsecond,row\n");
    assert_eq!(reader.members(), 3);

    let header = member(
        data,
        Header {
            extra: Some(vec![7; 300]),
            name: Some(b"sales.csv"),
            comment: Some(b"exported nightly"),
            header_crc: true,
            ..Header::default()
        },
    );
    assert_eq!(decode(&header).unwrap(), data);
    let mut both = header.clone();
    both.extend(member(b"tail\n", Header::default()));
    let mut expected = data.to_vec();
    expected.extend(b"tail\n");
    assert_eq!(decode(&both).unwrap(), expected);
}

#[test]
fn every_truncation_of_a_member_is_rejected() {
    let data = numbered_rows(200);
    for bytes in [
        gzip(&data),
        member(
            &data,
            Header {
                extra: Some(vec![1, 2, 3]),
                name: Some(b"n"),
                comment: Some(b"c"),
                header_crc: true,
                ..Header::default()
            },
        ),
    ] {
        for cut in 0..bytes.len() {
            assert!(is_corrupt(decode(&bytes[..cut])), "cut at {cut} accepted");
        }
        assert_eq!(decode(&bytes).unwrap(), data);
    }
}

#[test]
fn damaged_payloads_checksums_and_headers_are_rejected() {
    let data = numbered_rows(100);
    let valid = gzip(&data);
    // Any single damaged byte after the fixed header changes data, CRC or framing.
    for position in 10..valid.len() {
        let mut damaged = valid.clone();
        damaged[position] ^= 0xff;
        assert!(
            is_corrupt(decode(&damaged)),
            "damage at {position} accepted"
        );
    }
    let crc_at = valid.len() - 8;
    let mut bad_crc = valid.clone();
    bad_crc[crc_at] ^= 1;
    assert!(decode(&bad_crc).unwrap_err().to_string().contains("CRC-32"));
    let mut bad_size = valid.clone();
    *bad_size.last_mut().unwrap() ^= 1;
    assert!(decode(&bad_size).unwrap_err().to_string().contains("ISIZE"));

    let mut bad_header_crc = member(
        b"a\n",
        Header {
            name: Some(b"x"),
            header_crc: true,
            ..Header::default()
        },
    );
    bad_header_crc[12] ^= 1;
    assert!(
        decode(&bad_header_crc)
            .unwrap_err()
            .to_string()
            .contains("header checksum")
    );
    let reserved = member(
        b"a\n",
        Header {
            flags: 0x20,
            ..Header::default()
        },
    );
    assert!(
        decode(&reserved)
            .unwrap_err()
            .to_string()
            .contains("reserved")
    );
    let method = member(
        b"a\n",
        Header {
            method: Some(9),
            ..Header::default()
        },
    );
    assert!(
        decode(&method)
            .unwrap_err()
            .to_string()
            .contains("method 9")
    );
}

#[test]
fn a_damaged_or_truncated_later_member_and_trailing_bytes_are_rejected() {
    let first = gzip(b"a,b\n1,2\n");
    let second = gzip(&numbered_rows(50));
    for cut in 1..second.len() {
        let mut bytes = first.clone();
        bytes.extend(&second[..cut]);
        assert!(is_corrupt(decode(&bytes)), "second member cut at {cut}");
    }
    for position in 10..second.len() {
        let mut bytes = first.clone();
        bytes.extend(&second);
        bytes[first.len() + position] ^= 0xff;
        assert!(
            is_corrupt(decode(&bytes)),
            "second member damaged at {position}"
        );
    }
    for trailing in [&b"garbage"[..], b"\0", b"\0\0\0\0\0\0\0\0", b"\x1f", b"\n"] {
        let mut bytes = first.clone();
        bytes.extend(trailing);
        assert!(is_corrupt(decode(&bytes)), "trailing {trailing:?} accepted");
    }
    let mut bytes = first.clone();
    bytes.extend(b"x");
    assert!(
        decode(&bytes)
            .unwrap_err()
            .to_string()
            .contains("Unexpected data follows")
    );
    assert!(decode(b"").unwrap_err().to_string().contains("truncated"));
    assert!(
        decode(b"PK\x03\x04......")
            .unwrap_err()
            .to_string()
            .contains("signature")
    );
}

#[test]
fn isize_is_compared_modulo_four_gibibytes() {
    let crc = 0x1234_5678_u32;
    let mut trailer = [0_u8; 8];
    trailer[..4].copy_from_slice(&crc.to_le_bytes());
    trailer[4..].copy_from_slice(&5_u32.to_le_bytes());
    check_trailer(trailer, crc, 5).unwrap();
    check_trailer(trailer, crc, (1_u64 << 32) + 5).unwrap();
    check_trailer(trailer, crc, (3_u64 << 32) + 5).unwrap();
    assert!(check_trailer(trailer, crc, (1_u64 << 32) + 6).is_err());
    assert!(check_trailer(trailer, crc ^ 1, 5).is_err());
}

// ---- Bootstrap -----------------------------------------------------------------

#[test]
fn bootstrap_reads_first_rows_and_verifies_a_small_stream_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let data = b"name,note,amount\r\nAda,\"multi\nline, quoted\",1\r\nBo,\"say \"\"hi\"\"\",2\r\nragged\r\n";
    let path = write(dir.path(), "notes.csv.gz", &gzip(data));
    let bootstrap = CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap();
    assert_eq!(bootstrap.format(), CompressionFormat::Gzip);
    assert_eq!(bootstrap.path(), path);
    assert_eq!(bootstrap.encoding(), Encoding::Utf8);
    assert_eq!(bootstrap.dialect.delimiter, b',');
    assert!(bootstrap.dialect.has_header);
    assert!(bootstrap.reached_eof());
    assert_eq!(bootstrap.decompressed_size(), Some(data.len() as u64));
    assert_eq!(
        bootstrap.compressed_size(),
        fs::metadata(&path).unwrap().len()
    );
    assert_eq!(bootstrap.first_rows.len(), 4);
    assert_eq!(bootstrap.first_rows[1].fields[1], b"multi\nline, quoted");
    assert_eq!(bootstrap.first_rows[2].fields[1], b"say \"hi\"");
    assert_eq!(bootstrap.first_rows[3].fields, [b"ragged".to_vec()]);
    // Only the source exists: bootstrapping writes nothing to disk.
    assert_eq!(entries(dir.path()), [path.clone()]);

    for (name, bytes) in [
        ("bad-crc.gz", {
            let mut bytes = gzip(data);
            let at = bytes.len() - 8;
            bytes[at] ^= 1;
            bytes
        }),
        ("trailing.gz", {
            let mut bytes = gzip(data);
            bytes.extend(b"junk");
            bytes
        }),
        ("truncated.gz", {
            let bytes = gzip(data);
            bytes[..bytes.len() - 3].to_vec()
        }),
    ] {
        let path = write(dir.path(), name, &bytes);
        let error =
            CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap_err();
        assert!(
            matches!(error, QuarryError::CompressedFile { .. }),
            "{name}: {error:?}"
        );
    }
}

#[test]
fn bootstrap_stays_within_its_limit_and_detects_exact_end_of_data() {
    let dir = tempfile::tempdir().unwrap();
    let data = numbered_rows(200_000);
    let path = write(dir.path(), "big.csv.gz", &gzip(&data));
    let options = OpenOptions {
        rows: 10,
        sample_bytes: 1024,
        bootstrap_limit: 64 * 1024,
        ..OpenOptions::default()
    };
    let bootstrap = CompressedBootstrap::open(&path, options, None, None).unwrap();
    assert!(!bootstrap.reached_eof());
    assert_eq!(bootstrap.first_rows.len(), 10);
    assert_eq!(
        bootstrap.first_rows[9].fields,
        [b"8".to_vec(), b"v8".to_vec()]
    );
    assert!(bootstrap.metrics.bootstrap_bytes_read <= 64 * 1024);
    assert!(bootstrap.compressed_bytes_read() < fs::metadata(&path).unwrap().len());

    // Data that ends exactly with the requested rows is still verified to its end.
    let exact = write(dir.path(), "exact.csv.gz", &gzip(b"a,b\n1,2\n"));
    let bootstrap = CompressedBootstrap::open(
        &exact,
        OpenOptions {
            rows: 2,
            ..OpenOptions::default()
        },
        None,
        None,
    )
    .unwrap();
    assert!(bootstrap.reached_eof());
}

/// A valid 512-byte POSIX tar header for one four-byte file.
fn tar_header(name: &[u8]) -> Vec<u8> {
    let mut header = vec![0_u8; 512];
    header[..name.len()].copy_from_slice(name);
    header[100..108].copy_from_slice(b"0000644\0");
    header[108..116].copy_from_slice(b"0001750\0");
    header[116..124].copy_from_slice(b"0001750\0");
    header[124..136].copy_from_slice(b"00000000004\0");
    header[136..148].copy_from_slice(b"14701234567\0");
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[148..156].fill(b' ');
    let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    header
}

#[test]
fn bootstrap_rejects_tar_payloads_and_honours_the_decompressed_cap() {
    let dir = tempfile::tempdir().unwrap();
    let mut tar = tar_header(b"data.csv");
    tar.extend(b"a,b\n");
    tar.resize(2048, 0);
    assert!(looks_like_tar(&tar));
    let path = write(dir.path(), "archive.gz", &gzip(&tar));
    let error = CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap_err();
    assert!(reason(&error).contains("tar archive"));
    // The magic alone, without a matching checksum, is not a tar header.
    let mut damaged = tar_header(b"data.csv");
    damaged[0] ^= 1;
    assert!(!looks_like_tar(&damaged));
    assert!(!looks_like_tar(&tar[..511]));

    // Delimited text that merely holds "ustar " at offset 257 opens normally.
    let mut text = b"a,".to_vec();
    text.extend(std::iter::repeat_n(b'x', 255));
    text.extend(b"ustar value\n");
    assert_eq!(&text[257..263], b"ustar ");
    for row in 0..100 {
        text.extend(format!("{row},{row}\n").as_bytes());
    }
    assert!(text.len() > 512);
    let path = write(dir.path(), "ustar.csv.gz", &gzip(&text));
    let bootstrap = CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap();
    assert!(bootstrap.reached_eof());
    assert_eq!(
        bootstrap.first_rows[0].fields[1],
        [vec![b'x'; 255], b"ustar value".to_vec()].concat()
    );

    let bomb = write(dir.path(), "bomb.csv.gz", &gzip(&b"0,0\n".repeat(1 << 20)));
    let error = CompressedBootstrap::open(
        &bomb,
        OpenOptions {
            rows: 1_000_000,
            ..OpenOptions::default()
        },
        None,
        Some(64 * 1024),
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            QuarryError::DecompressionLimitExceeded {
                limit_bytes: 65_536,
                decompressed_bytes,
                ..
            } if decompressed_bytes > 65_536
        ),
        "{error:?}"
    );
    assert!(matches!(
        CompressedBootstrap::open(&bomb, OpenOptions::default(), None, Some(0)),
        Err(QuarryError::InvalidOption(_))
    ));
}

#[test]
fn bootstrap_and_full_open_match_plain_files_across_encodings() {
    let dir = tempfile::tempdir().unwrap();
    let text = "name;city\r\n\"Zoë, \"\"Z\"\"\";Málaga\r\n\"two\nlines\";x\r\nragged\r\n";
    let utf16le: Vec<u8> = [0xff, 0xfe]
        .into_iter()
        .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    let utf16be: Vec<u8> = [0xfe, 0xff]
        .into_iter()
        .chain(text.encode_utf16().flat_map(u16::to_be_bytes))
        .collect();
    let cp1252: Vec<u8> = text
        .chars()
        .map(|c| match c {
            'ë' => 0xeb,
            'á' => 0xe1,
            c => c as u8,
        })
        .collect();
    let cases = [
        ("utf8", text.as_bytes().to_vec(), None, Encoding::Utf8),
        (
            "bom",
            [b"\xef\xbb\xbf".as_slice(), text.as_bytes()].concat(),
            None,
            Encoding::Utf8,
        ),
        ("utf16le", utf16le, None, Encoding::Utf16Le),
        ("utf16be", utf16be, None, Encoding::Utf16Be),
        (
            "cp1252",
            cp1252,
            Some(Encoding::Windows1252),
            Encoding::Windows1252,
        ),
    ];
    for (name, bytes, requested, expected) in cases {
        let plain_path = write(dir.path(), &format!("{name}.csv"), &bytes);
        let gz_path = write(dir.path(), &format!("{name}.csv.gz"), &gzip(&bytes));
        let temp = tempfile::tempdir().unwrap();
        let (expected_session, expected_index) =
            plain(&plain_path, OpenOptions::default(), requested);
        let (session, index) = open_compressed(
            &gz_path,
            OpenOptions::default(),
            requested,
            CompressedOpenConfig {
                index_config: IndexConfig {
                    chunk_bytes: 3,
                    checkpoint_every: 2,
                    memory_budget_bytes: 1024,
                },
                ..config(temp.path())
            },
            WorkingCopyCleanup::default(),
        )
        .unwrap();
        assert_eq!(session.encoding(), expected, "{name}");
        assert_eq!(session.dialect.delimiter, b';', "{name}");
        assert_eq!(
            session.dialect.has_header,
            expected_session.dialect.has_header
        );
        assert_eq!(session.first_rows, expected_session.first_rows, "{name}");
        assert_eq!(
            index.indexed_rows(),
            expected_index.indexed_rows(),
            "{name}"
        );
        assert_eq!(
            index.maximum_width(),
            expected_index.maximum_width(),
            "{name}"
        );
        assert_eq!(
            all_rows(&session, &index),
            all_rows(&expected_session, &expected_index),
            "{name}"
        );
        assert_eq!(fs::read(session.path()).unwrap(), bytes, "{name}");
        drop(session);
        assert!(
            entries(temp.path()).is_empty(),
            "{name} left temporary files"
        );
    }
}

// ---- Background decompression ---------------------------------------------------

#[test]
fn completes_in_one_pass_with_a_private_owned_copy() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let data = numbered_rows(5_000);
    // Split mid-record across members: concatenation must be seamless.
    let mut bytes = gzip(&data[..1234]);
    bytes.extend(gzip(&data[1234..]));
    let path = write(source_dir.path(), "rows.csv.gz", &bytes);
    let cleanup = WorkingCopyCleanup::default();
    let bootstrap = CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap();
    let job = bootstrap
        .start_decompression(config(temp.path()), cleanup.clone())
        .unwrap();
    wait_for(|| job.progress().done, "decompression");
    let progress = job.progress();
    assert!(!progress.cancelled);
    assert_eq!(progress.decompressed_bytes_written, data.len() as u64);
    assert_eq!(progress.compressed_total_bytes, bytes.len() as u64);
    assert_eq!(progress.compressed_bytes_read, bytes.len() as u64);
    assert_eq!(progress.rows_scanned, 5_001);
    assert_eq!(progress.columns_found, 2);
    assert!(job.error().is_none());
    let CompressedOpenOutcome::Complete { session, index } = job.wait().unwrap() else {
        panic!("decompression was cancelled");
    };
    assert!(session.is_compressed());
    assert_eq!(session.compression(), Some(CompressionFormat::Gzip));
    assert_eq!(session.compressed_source_path(), Some(path.as_path()));
    assert_eq!(session.compressed_file_size(), Some(bytes.len() as u64));
    assert_eq!(session.file_size, data.len() as u64);
    assert_ne!(session.path(), path);
    assert!(session.path().starts_with(temp.path()));
    assert_eq!(fs::read(session.path()).unwrap(), data);
    assert_eq!(index.indexed_rows(), 5_001);
    assert_eq!(index.indexed_bytes(), data.len() as u64);
    // The completed count is adopted, so no second indexing pass is needed.
    assert_eq!(session.completed_record_count(), Some(5_001));
    let rows = session.read_rows(&index, 4_000, 2).unwrap();
    assert_eq!(rows[0].fields, [b"3999".to_vec(), b"v3999".to_vec()]);
    session.ensure_compressed_source_unchanged().unwrap();
    session.ensure_source_unchanged().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root = session.path().parent().unwrap();
        assert_eq!(
            fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(session.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let backing = session.path().to_path_buf();
    drop(session);
    assert!(!backing.exists());
    assert!(entries(temp.path()).is_empty());
    assert!(cleanup.take_errors().is_empty());
    // The compressed source is untouched.
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn empty_compressed_data_opens_as_an_empty_document() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = write(source_dir.path(), "empty.csv.gz", &gzip(b""));
    let (session, index) = open_compressed(
        &path,
        OpenOptions::default(),
        None,
        config(temp.path()),
        WorkingCopyCleanup::default(),
    )
    .unwrap();
    assert_eq!(session.file_size, 0);
    assert!(session.first_rows.is_empty());
    assert_eq!(index.indexed_rows(), 0);
}

fn slow_job(source_dir: &Path, temp: &Path, rows: usize) -> (PathBuf, CompressedDecompressJob) {
    let path = write(source_dir, "live.csv.gz", &gzip(&numbered_rows(rows)));
    let job = start_slow(&path, temp);
    (path, job)
}

/// Like `slow_job`, with the worker held at `checkpoint` until the pause is released.
fn paused_job(
    source_dir: &Path,
    temp: &Path,
    rows: usize,
    checkpoint: Checkpoint,
) -> (PathBuf, CompressedDecompressJob, Pause) {
    let path = write(source_dir, "live.csv.gz", &gzip(&numbered_rows(rows)));
    let pause = Pause::new(&path, checkpoint);
    let job = start_slow(&path, temp);
    (path, job, pause)
}

/// Decompress through tiny chunks with a small bootstrap.
fn start_slow(path: &Path, temp: &Path) -> CompressedDecompressJob {
    let bootstrap = CompressedBootstrap::open(
        path,
        OpenOptions {
            rows: 5,
            sample_bytes: 1024,
            bootstrap_limit: 64 * 1024,
            ..OpenOptions::default()
        },
        None,
        None,
    )
    .unwrap();
    bootstrap
        .start_decompression(
            CompressedOpenConfig {
                index_config: IndexConfig {
                    chunk_bytes: 8,
                    checkpoint_every: 64,
                    memory_budget_bytes: 64 * 1024,
                },
                ..config(temp)
            },
            WorkingCopyCleanup::default(),
        )
        .unwrap()
}

/// The worker is held once it has published at least this many bytes of rows.
const LIVE_ROWS: Checkpoint = Checkpoint::Published {
    min_bytes: 16 * 1024,
};

#[test]
fn reads_rows_live_and_cancellation_removes_every_temporary_file() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (_, job, pause) = paused_job(source_dir.path(), temp.path(), 300_000, LIVE_ROWS);
    assert!(pause.wait_for(1), "the worker published no rows");
    let progress = job.progress();
    assert!(!progress.done);
    assert!(progress.rows_scanned >= 1_000, "{progress:?}");
    assert!(!entries(temp.path()).is_empty());
    let snapshot = job.snapshot();
    assert!(snapshot.indexed_bytes() <= job.progress().decompressed_bytes_written);
    let rows = job.read_rows(500, 3).unwrap();
    assert_eq!(rows[0].fields, [b"499".to_vec(), b"v499".to_vec()]);
    assert_eq!(rows[2].fields, [b"501".to_vec(), b"v501".to_vec()]);
    assert!(matches!(
        job.read_rows(snapshot.indexed_rows() + 1_000_000, 1),
        Err(QuarryError::RowNotIndexed { .. })
    ));
    let started = Instant::now();
    job.cancel();
    assert!(matches!(
        job.wait().unwrap(),
        CompressedOpenOutcome::Cancelled
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(entries(temp.path()).is_empty());
}

#[test]
fn live_reads_stop_at_the_published_index_boundary() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (_, job, pause) = paused_job(source_dir.path(), temp.path(), 300_000, LIVE_ROWS);
    assert!(pause.wait_for(1), "the worker published no rows");
    let snapshot = job.snapshot();
    let indexed = snapshot.indexed_rows();
    // Malformed bytes past the boundary, as a write the index has not reached yet.
    let mut copy = fs::OpenOptions::new()
        .append(true)
        .open(&job.backing_path)
        .unwrap();
    copy.write_all(b"\n\"bad\"x,1\n\"unterminated").unwrap();
    drop(copy);
    assert!(fs::metadata(&job.backing_path).unwrap().len() > snapshot.indexed_bytes());
    // The last two indexed records hold ids `indexed - 3` and `indexed - 2`.
    let rows = job.read_rows(indexed - 2, 2).unwrap();
    assert_eq!(rows.len(), 2);
    for (row, id) in rows.iter().zip([indexed - 3, indexed - 2]) {
        assert_eq!(
            row.fields,
            [id.to_string().into_bytes(), format!("v{id}").into_bytes()]
        );
    }
    job.cancel();
    assert!(matches!(
        job.wait().unwrap(),
        CompressedOpenOutcome::Cancelled
    ));
    assert!(entries(temp.path()).is_empty());
}

#[test]
fn dropping_an_active_job_removes_its_private_copy() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (_, job) = slow_job(source_dir.path(), temp.path(), 300_000);
    wait_for(|| job.progress().rows_scanned >= 100, "progress");
    drop(job);
    assert!(entries(temp.path()).is_empty());
}

#[test]
fn a_source_changed_mid_decompression_is_rejected_and_cleaned_up() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (path, job, pause) = paused_job(
        source_dir.path(),
        temp.path(),
        100_000,
        Checkpoint::BeforeSourceCheck,
    );
    // The copy is complete and held just before its final source check.
    assert!(pause.wait_for(1), "the worker never finished copying");
    fs::rename(&path, source_dir.path().join("moved.gz")).unwrap();
    fs::write(&path, gzip(b"different,source\n")).unwrap();
    pause.release();
    assert!(matches!(job.wait(), Err(QuarryError::SourceChanged)));
    assert!(entries(temp.path()).is_empty());

    // A change between bootstrap and start is caught before any copy is made.
    let path = write(source_dir.path(), "late.csv.gz", &gzip(b"a,b\n1,2\n"));
    let bootstrap = CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap();
    fs::write(&path, gzip(b"a,b\n1,2\n3,4\n")).unwrap();
    assert!(matches!(
        bootstrap.start_decompression(config(temp.path()), WorkingCopyCleanup::default()),
        Err(QuarryError::SourceChanged)
    ));
    assert!(entries(temp.path()).is_empty());
}

#[test]
fn a_decompression_bomb_stops_at_the_cap_without_leaving_files() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let data = b"0,0\n".repeat(2 << 20); // 8 MiB that compresses to a few KiB
    let bytes = gzip(&data);
    assert!(bytes.len() < 64 * 1024);
    let path = write(source_dir.path(), "bomb.csv.gz", &bytes);
    let bootstrap = CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap();
    let limit = 1 << 20;
    let job = bootstrap
        .start_decompression(
            CompressedOpenConfig {
                max_decompressed_bytes: Some(limit),
                ..config(temp.path())
            },
            WorkingCopyCleanup::default(),
        )
        .unwrap();
    wait_for(|| job.progress().done, "cap");
    assert!(job.progress().decompressed_bytes_written <= limit);
    assert!(job.error().unwrap().contains("limit"));
    let error = job.wait().unwrap_err();
    assert!(
        matches!(
            error,
            QuarryError::DecompressionLimitExceeded { limit_bytes, decompressed_bytes, .. }
                if limit_bytes == limit && decompressed_bytes > limit
        ),
        "{error:?}"
    );
    assert!(entries(temp.path()).is_empty());
}

#[test]
fn free_space_below_the_reserve_stops_decompression_and_cleans_up() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = write(
        source_dir.path(),
        "rows.csv.gz",
        &gzip(&numbered_rows(1_000)),
    );
    let bootstrap = CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap();
    let error = expect_err(bootstrap.start_decompression(
        CompressedOpenConfig {
            min_free_space_reserve_bytes: u64::MAX,
            space_check_interval_bytes: 1,
            ..config(temp.path())
        },
        WorkingCopyCleanup::default(),
    ));
    assert!(
        matches!(
            &error,
            QuarryError::InsufficientStorage { directory, required_bytes: u64::MAX, .. }
                if directory == temp.path()
        ),
        "{error:?}"
    );
    assert!(entries(temp.path()).is_empty());
    assert!(matches!(
        bootstrap.start_decompression(
            CompressedOpenConfig {
                space_check_interval_bytes: 0,
                ..config(temp.path())
            },
            WorkingCopyCleanup::default(),
        ),
        Err(QuarryError::InvalidOption(_))
    ));
}

#[test]
fn free_space_checks_use_the_output_size_and_keep_the_reserve_before_writing() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let cleanup = WorkingCopyCleanup::default();
    // Many empty members make the compressed file far larger than its output.
    let data = b"a,b\n1,2\n";
    let mut bytes = gzip(data);
    for _ in 0..1_000 {
        bytes.extend(gzip(b""));
    }
    assert!(bytes.len() > 10_000);
    let path = write(source_dir.path(), "members.csv.gz", &bytes);
    let tight = CompressedOpenConfig {
        available_space_override: Some(100),
        ..config(temp.path())
    };
    let (session, index) = open_compressed(
        &path,
        OpenOptions::default(),
        None,
        tight.clone(),
        cleanup.clone(),
    )
    .unwrap();
    assert_eq!(session.file_size, data.len() as u64);
    assert_eq!(index.indexed_rows(), 2);
    drop(session);
    let copy = CompressedRawDecompressJob::start(&path, tight, cleanup.clone())
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
    assert_eq!(fs::read(copy.path()).unwrap(), data);
    drop(copy);
    assert!(entries(temp.path()).is_empty());

    // A copy shorter than one check interval still keeps the reserve: its first
    // write is refused, so nothing is written.
    let rows = write(source_dir.path(), "rows.csv.gz", &gzip(&numbered_rows(100)));
    let reserved = CompressedOpenConfig {
        min_free_space_reserve_bytes: 990,
        available_space_override: Some(1_000),
        ..config(temp.path())
    };
    let job = CompressedRawDecompressJob::start(&rows, reserved.clone(), cleanup.clone()).unwrap();
    wait_for(|| job.progress().done, "the reserve check");
    assert_eq!(job.progress().decompressed_bytes_written, 0);
    let error = job.wait().unwrap_err();
    assert!(
        matches!(
            &error,
            QuarryError::InsufficientStorage { directory, required_bytes, available_bytes: 1_000 }
                if directory == temp.path() && *required_bytes > 1_000
        ),
        "{error:?}"
    );
    assert!(entries(temp.path()).is_empty());

    // With the size verified, the whole output and the reserve are required up front.
    let bootstrap = CompressedBootstrap::open(&rows, OpenOptions::default(), None, None).unwrap();
    let exact = bootstrap.decompressed_size().unwrap();
    let error = expect_err(bootstrap.start_decompression(reserved, cleanup.clone()));
    assert!(
        matches!(
            error,
            QuarryError::InsufficientStorage { required_bytes, .. } if required_bytes == 990 + exact
        ),
        "{error:?}"
    );
    assert!(entries(temp.path()).is_empty());
    assert!(cleanup.take_errors().is_empty());
}

#[test]
fn corruption_after_the_bootstrap_fails_the_job_without_leaving_files() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let options = OpenOptions {
        rows: 5,
        sample_bytes: 1024,
        bootstrap_limit: 64 * 1024,
        ..OpenOptions::default()
    };
    let valid = gzip(&numbered_rows(200_000));
    let mut cases = Vec::new();
    let mut bad_crc = valid.clone();
    let at = bad_crc.len() - 8;
    bad_crc[at] ^= 1;
    cases.push(("crc", bad_crc));
    cases.push(("truncated", valid[..valid.len() - 100].to_vec()));
    let mut trailing = valid.clone();
    trailing.extend(b"not gzip");
    cases.push(("trailing", trailing));
    let mut second = valid.clone();
    let tail = gzip(b"more\n");
    second.extend(&tail[..tail.len() - 1]);
    cases.push(("second member", second));
    for (name, bytes) in cases {
        let path = write(source_dir.path(), &format!("{name}.gz"), &bytes);
        let bootstrap = CompressedBootstrap::open(&path, options, None, None).unwrap();
        assert!(!bootstrap.reached_eof(), "{name}");
        let error = bootstrap
            .start_decompression(config(temp.path()), WorkingCopyCleanup::default())
            .unwrap()
            .wait()
            .unwrap_err();
        assert!(
            matches!(error, QuarryError::CompressedFile { .. }),
            "{name}: {error:?}"
        );
        assert!(entries(temp.path()).is_empty(), "{name}");
    }
}

#[cfg(unix)]
#[test]
fn cleanup_follows_the_pinned_directory_after_it_is_renamed() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = write(source_dir.path(), "rows.csv.gz", &gzip(&numbered_rows(10)));
    let cleanup = WorkingCopyCleanup::default();
    let (session, _) = open_compressed(
        &path,
        OpenOptions::default(),
        None,
        config(temp.path()),
        cleanup.clone(),
    )
    .unwrap();
    let root = session.path().parent().unwrap().to_path_buf();
    let moved = temp.path().join("moved");
    fs::rename(&root, &moved).unwrap();
    fs::create_dir_all(root.join("foreign")).unwrap();
    fs::write(root.join("foreign/keep.csv"), b"preserve").unwrap();
    drop(session);
    assert!(!moved.exists(), "the renamed private copy survived");
    assert_eq!(
        fs::read(root.join("foreign/keep.csv")).unwrap(),
        b"preserve"
    );
    assert!(cleanup.take_errors().is_empty());
}

// ---- Read-only contract and existing engines -----------------------------------

#[test]
fn compressed_documents_are_read_only_and_copies_are_uncompressed() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let data = b"id,flag\n1,keep\n2,drop\n\"3\nx\",keep\n";
    // A gzip source named .csv proves aliasing is checked by path, not by suffix.
    let path = write(source_dir.path(), "data.csv", &gzip(data));
    let (session, _) = open_compressed(
        &path,
        OpenOptions::default(),
        None,
        config(temp.path()),
        WorkingCopyCleanup::default(),
    )
    .unwrap();
    assert!(matches!(
        session.start_save_with_edits(BTreeMap::new(), BTreeMap::new()),
        Err(QuarryError::CompressedReadOnly)
    ));
    assert!(matches!(
        session.start_save_with_header_renames(BTreeMap::from([(0, b"ID".to_vec())])),
        Err(QuarryError::CompressedReadOnly)
    ));
    let plain_path = write(source_dir.path(), "plain.csv", data);
    let (plain_session, _) = plain(&plain_path, OpenOptions::default(), None);
    assert!(matches!(
        plain_session.start_save_to_original(BTreeMap::new(), BTreeMap::new(), &session),
        Err(QuarryError::CompressedReadOnly)
    ));
    assert!(
        QuarryError::CompressedReadOnly
            .to_string()
            .contains("Save Copy As")
    );

    for destination in [out.path().join("copy.csv.gz"), out.path().join("COPY.GZ")] {
        assert!(matches!(
            session.start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), &destination),
            Err(QuarryError::InvalidOption(message)) if message.contains("uncompressed")
        ));
        assert!(matches!(
            session.start_save_as_utf8(BTreeMap::new(), BTreeMap::new(), None, &destination),
            Err(QuarryError::InvalidOption(_))
        ));
        assert!(!destination.exists());
    }
    assert!(matches!(
        session.start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), &path),
        Err(QuarryError::ExportDestinationIsSource)
    ));

    let copy = out.path().join("copy.csv");
    let SaveAsOutcome::Complete(_) = session
        .start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), &copy)
        .unwrap()
        .wait()
        .unwrap()
    else {
        panic!("save copy was cancelled");
    };
    assert_eq!(fs::read(&copy).unwrap(), data);

    let edited = out.path().join("edited.csv");
    let expected_edited = out.path().join("expected-edited.csv");
    let edits = BTreeMap::from([((2, 1), b"changed".to_vec())]);
    session
        .start_save_as_with_edits(BTreeMap::new(), edits.clone(), &edited)
        .unwrap()
        .wait()
        .unwrap();
    plain_session
        .start_save_as_with_edits(BTreeMap::new(), edits, &expected_edited)
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(
        fs::read(&edited).unwrap(),
        fs::read(&expected_edited).unwrap()
    );

    let query = FilterQuery {
        predicates: vec![FilterPredicate {
            column: 1,
            operator: FilterOperator::Equals,
            value: b"keep".to_vec(),
            upper_bound: None,
        }],
        case_sensitivity: CaseSensitivity::Sensitive,
    };
    let filter = session.start_filter(query.clone()).unwrap().wait().unwrap();
    let expected_filter = plain_session
        .start_filter(query.clone())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(filter.matches_found(), 2);
    assert_eq!(filter.matches_found(), expected_filter.matches_found());
    assert!(matches!(
        session.start_filtered_export(query.clone(), out.path().join("kept.csv.gz")),
        Err(QuarryError::InvalidOption(_))
    ));
    let filtered = out.path().join("kept.csv");
    let expected_filtered = out.path().join("expected-kept.csv");
    let FilterExportOutcome::Complete(_) = session
        .start_filtered_export(query.clone(), &filtered)
        .unwrap()
        .wait()
        .unwrap()
    else {
        panic!("filtered export was cancelled");
    };
    plain_session
        .start_filtered_export(query, &expected_filtered)
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(
        fs::read(&filtered).unwrap(),
        b"id,flag\n1,keep\n\"3\nx\",keep\n"
    );
    assert_eq!(
        fs::read(&filtered).unwrap(),
        fs::read(&expected_filtered).unwrap()
    );
    // Plain documents get the same uncompressed-output rule.
    assert!(matches!(
        plain_session.start_save_as_with_edits(
            BTreeMap::new(),
            BTreeMap::new(),
            out.path().join("plain.csv.gz")
        ),
        Err(QuarryError::InvalidOption(_))
    ));
    assert_eq!(fs::read(&path).unwrap(), gzip(data));
}

#[test]
fn reopening_the_private_copy_changes_format_without_decompressing_again() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let data = b"a;b,c\n1;2,3\n4;5,6\n";
    let path = write(source_dir.path(), "mixed.csv.gz", &gzip(data));
    let (session, _) = open_compressed(
        &path,
        OpenOptions::default(),
        None,
        config(temp.path()),
        WorkingCopyCleanup::default(),
    )
    .unwrap();
    assert_eq!(session.dialect.delimiter, b',');
    let reopened = session
        .reopen_compressed_backing(
            OpenOptions {
                delimiter: Some(b';'),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
            None,
        )
        .unwrap();
    assert!(reopened.is_compressed());
    assert_eq!(reopened.path(), session.path());
    assert_eq!(reopened.compressed_source_path(), Some(path.as_path()));
    assert!(!reopened.dialect.has_header);
    assert_eq!(
        reopened.first_rows[0].fields,
        [b"a".to_vec(), b"b,c".to_vec()]
    );
    let index = reopened
        .start_indexing(IndexConfig::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(index.indexed_rows(), 3);
    // The private copy stays alive while either document holds it.
    let backing = session.path().to_path_buf();
    drop(session);
    assert!(backing.exists());
    assert_eq!(
        reopened.read_rows(&index, 2, 1).unwrap()[0].fields[1],
        b"5,6"
    );

    fs::write(&path, gzip(b"changed\n")).unwrap();
    assert!(matches!(
        reopened.reopen_compressed_backing(OpenOptions::default(), None),
        Err(QuarryError::SourceChanged)
    ));
    assert!(matches!(
        reopened.ensure_compressed_source_unchanged(),
        Err(QuarryError::SourceChanged)
    ));
    let plain_path = write(source_dir.path(), "plain.csv", data);
    assert!(matches!(
        Session::open(&plain_path, OpenOptions::default())
            .unwrap()
            .reopen_compressed_backing(OpenOptions::default(), None),
        Err(QuarryError::InvalidOption(_))
    ));
    drop(reopened);
    assert!(!backing.exists());
}

#[test]
fn inspection_reads_the_private_copy_and_recovery_rejects_gzip_names() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let data = numbered_rows(50);
    let path = write(source_dir.path(), "rows.csv.gz", &gzip(&data));
    let (session, index) = open_compressed(
        &path,
        OpenOptions::default(),
        None,
        config(temp.path()),
        WorkingCopyCleanup::default(),
    )
    .unwrap();
    let report = InspectionJob::start(
        session.path(),
        InspectionOptions::default(),
        InspectionEdits::default(),
    )
    .unwrap()
    .wait()
    .unwrap();
    let plain_path = write(source_dir.path(), "rows.csv", &data);
    let expected = InspectionJob::start(
        &plain_path,
        InspectionOptions::default(),
        InspectionEdits::default(),
    )
    .unwrap()
    .wait()
    .unwrap();
    assert_eq!(report.status, InspectionStatus::Complete);
    assert_eq!(report.total_findings, 0);
    assert_eq!(report.records, expected.records);
    assert_eq!(report.delimiter, b',');
    assert!(report.records >= index.indexed_rows() - 1);
    assert_eq!(report.total_bytes, data.len() as u64);
    let destination = source_dir.path().join("recovered.csv.gz");
    assert!(matches!(
        RecoveryJob::start(&report, &BTreeSet::new(), &destination),
        Err(QuarryError::InvalidOption(_))
    ));
    assert!(!destination.exists());
}

#[test]
fn combine_and_join_explain_that_compressed_inputs_are_not_supported_yet() {
    let dir = tempfile::tempdir().unwrap();
    let gz = write(dir.path(), "left.csv.gz", &gzip(b"id,a\n1,x\n"));
    let csv = write(dir.path(), "right.csv", b"id,a\n2,y\n");
    let zst = write(dir.path(), "other.zst", b"\x28\xb5\x2f\xfd\0\0");

    let error = expect_err(
        check_combine_files(vec![csv.clone(), gz.clone()], CombineOptions::default())
            .unwrap()
            .wait(),
    );
    assert!(
        matches!(&error, QuarryError::CombineFile { path, reason, .. }
            if path == &gz && reason.contains("Combine Files") && reason.contains("Save Copy As")),
        "{error:?}"
    );
    let error = expect_err(
        check_combine_files(vec![csv.clone(), zst.clone()], CombineOptions::default())
            .unwrap()
            .wait(),
    );
    assert!(error.to_string().contains("Zstandard"), "{error}");

    let error = expect_err(read_join_header(gz.clone()).unwrap().wait());
    assert!(
        matches!(&error, QuarryError::JoinFile { reason, .. } if reason.contains("Join Files")),
        "{error:?}"
    );
    let input = |path: &Path| JoinInput {
        path: path.to_path_buf(),
        delimiter: None,
        has_header: true,
        encoding: None,
        key_columns: vec![0],
        expected_version: None,
        require_unique: false,
    };
    let temp = tempfile::tempdir().unwrap();
    let error = expect_err(
        analyze_join(
            [input(&csv), input(&gz)],
            temp.path().to_path_buf(),
            JoinConfig::default(),
        )
        .unwrap()
        .wait(),
    );
    assert!(error.to_string().contains("Join Files"), "{error}");
}

#[test]
fn a_preview_session_shows_bootstrap_rows_and_reads_nothing() {
    let source_dir = tempfile::tempdir().unwrap();
    let data = b"name,value\nalpha,1\nbeta,2\n";
    let path = write(source_dir.path(), "preview.csv.gz", &gzip(data));
    let bootstrap = CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap();
    let preview = bootstrap.preview_session();
    assert!(preview.is_compressed());
    assert_eq!(preview.compression(), Some(CompressionFormat::Gzip));
    assert_eq!(preview.compressed_source_path(), Some(path.as_path()));
    assert_eq!(
        preview.compressed_file_size(),
        Some(bootstrap.compressed_size())
    );
    assert_eq!(preview.path(), Path::new(""));
    assert_eq!(preview.file_size, 0);
    assert_eq!(preview.first_rows, bootstrap.first_rows);
    assert_eq!(preview.dialect.delimiter, bootstrap.dialect.delimiter);
    assert_eq!(preview.dialect.has_header, bootstrap.dialect.has_header);
    assert_eq!(preview.encoding(), bootstrap.encoding());

    // Nothing can read the compressed bytes as text or write over the source.
    assert!(
        preview
            .start_indexing(IndexConfig::default())
            .and_then(|job| job.wait())
            .is_err()
    );
    assert!(matches!(
        preview.start_save_with_edits(BTreeMap::new(), BTreeMap::new()),
        Err(QuarryError::CompressedReadOnly)
    ));
    let destination = source_dir.path().join("copy.csv");
    assert!(
        preview
            .start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), &destination)
            .and_then(|job| job.wait())
            .is_err()
    );
    assert!(!destination.exists());
    assert!(
        preview
            .reopen_compressed_backing(OpenOptions::default(), None)
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), gzip(data));
    assert_eq!(entries(source_dir.path()), vec![path]);
}

// ---- Bounded headers, cancellation, exact limits and raw copies ----------------

/// A member whose filename or comment never ends within the cap, followed by
/// megabytes of non-NUL bytes that an unbounded reader would scan for a NUL.
fn oversized_header_text(flag: u8) -> Vec<u8> {
    let mut bytes = vec![0x1f, 0x8b, 8, flag, 0, 0, 0, 0, 0, 255];
    bytes.extend(std::iter::repeat_n(b'n', 8 << 20));
    bytes
}

/// Sets a cancellation flag once `after` bytes have been read from it.
struct TripReader<'a> {
    inner: io::Cursor<&'a [u8]>,
    cancel: Arc<AtomicBool>,
    after: u64,
}

impl Read for TripReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.inner.position() >= self.after {
            self.cancel.store(true, Ordering::Release);
        }
        self.inner.read(buffer)
    }
}

#[test]
fn oversized_header_text_fails_fast_in_bootstrap_and_decompression() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let options = OpenOptions {
        rows: 5,
        sample_bytes: 1024,
        bootstrap_limit: 64 * 1024,
        ..OpenOptions::default()
    };
    let body = gzip(&numbered_rows(200_000));
    for (flag, field) in [(FLAG_NAME, "filename"), (FLAG_COMMENT, "comment")] {
        let expected = format!("The gzip header {field} exceeds the 65536-byte limit.");

        // Text exactly at the cap is still valid gzip.
        let text = vec![b'n'; MAX_GZIP_HEADER_TEXT];
        let header = if flag == FLAG_NAME {
            Header {
                name: Some(&text),
                ..Header::default()
            }
        } else {
            Header {
                comment: Some(&text),
                ..Header::default()
            }
        };
        assert_eq!(decode(&member(b"a,b\n", header)).unwrap(), b"a,b\n");

        // The reader stops just past the cap instead of scanning 8 MiB for a NUL.
        let oversized = oversized_header_text(flag);
        let mut reader = GzipMembers::new(BufReader::with_capacity(4096, oversized.as_slice()));
        let error = reader.read(&mut [0_u8; 64]).unwrap_err();
        let reason_text = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<CorruptGzip>())
            .map(|corrupt| corrupt.0.clone());
        assert_eq!(reason_text.as_deref(), Some(expected.as_str()));
        let consumed = oversized.len() - reader.input.get_ref().len();
        assert!(
            consumed <= 10 + MAX_GZIP_HEADER_TEXT + 1 + 4096,
            "{consumed}"
        );

        // In the bootstrap: the first member's header.
        let path = write(source_dir.path(), &format!("{field}.csv.gz"), &oversized);
        let error =
            CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap_err();
        assert_eq!(reason(&error), expected);

        // In decompression: a later member the bootstrap never reaches.
        let mut bytes = body.clone();
        bytes.extend(&oversized);
        let path = write(source_dir.path(), &format!("late-{field}.csv.gz"), &bytes);
        let bootstrap = CompressedBootstrap::open(&path, options, None, None).unwrap();
        assert!(!bootstrap.reached_eof());
        let error = bootstrap
            .start_decompression(config(temp.path()), WorkingCopyCleanup::default())
            .unwrap()
            .wait()
            .unwrap_err();
        assert_eq!(reason(&error), expected);
        assert!(entries(temp.path()).is_empty(), "{field}");
        let error = CompressedRawDecompressJob::start(
            &path,
            config(temp.path()),
            WorkingCopyCleanup::default(),
        )
        .unwrap()
        .wait()
        .unwrap_err();
        assert_eq!(reason(&error), expected);
        assert!(entries(temp.path()).is_empty(), "{field}");
    }
}

#[test]
fn cancellation_stops_at_member_boundaries_and_inside_long_headers() {
    // Between members: the next header is never read once cancelled.
    let cancel = Arc::new(AtomicBool::new(false));
    let mut bytes = gzip(b"first\n");
    bytes.extend(gzip(b"second\n"));
    let mut reader =
        GzipMembers::new(BufReader::new(bytes.as_slice())).with_cancel(Arc::clone(&cancel));
    let mut buffer = [0_u8; 64];
    assert_eq!(reader.read(&mut buffer).unwrap(), 6);
    assert_eq!(reader.members(), 1);
    cancel.store(true, Ordering::Release);
    assert!(is_cancelled(&reader.read(&mut buffer).unwrap_err()));
    assert_eq!(reader.members(), 1);

    // Inside a long (valid) filename: stopped within one check interval.
    let name = vec![b'n'; 60 * 1024];
    let long = member(
        b"a,b\n",
        Header {
            name: Some(&name),
            ..Header::default()
        },
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let trip = 10 + 1_000;
    let mut reader = GzipMembers::new(BufReader::with_capacity(
        64,
        TripReader {
            inner: io::Cursor::new(long.as_slice()),
            cancel: Arc::clone(&cancel),
            after: trip,
        },
    ))
    .with_cancel(cancel);
    assert!(is_cancelled(&reader.read(&mut buffer).unwrap_err()));
    let position = reader.input.get_ref().inner.position();
    assert!(
        position <= trip + HEADER_CANCEL_CHECK_BYTES as u64 + 64,
        "read {position} bytes after cancelling at {trip}"
    );
}

#[test]
fn cancelling_while_crossing_members_stops_promptly_without_leftovers() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let small = gzip(&b"1,x\n".repeat(16));
    let mut bytes = gzip(b"id,v\n");
    for _ in 0..50_000 {
        bytes.extend(&small);
    }
    let path = write(source_dir.path(), "members.csv.gz", &bytes);
    let pause = Pause::new(&path, Checkpoint::Published { min_bytes: 4_096 });
    let slow = CompressedOpenConfig {
        index_config: IndexConfig {
            chunk_bytes: 8,
            checkpoint_every: 64,
            memory_budget_bytes: 64 * 1024,
        },
        ..config(temp.path())
    };
    let bootstrap = CompressedBootstrap::open(
        &path,
        OpenOptions {
            rows: 5,
            sample_bytes: 1024,
            bootstrap_limit: 64 * 1024,
            ..OpenOptions::default()
        },
        None,
        None,
    )
    .unwrap();
    let job = bootstrap
        .start_decompression(slow.clone(), WorkingCopyCleanup::default())
        .unwrap();
    assert!(pause.wait_for(1), "no members were decompressed");
    assert!(!job.progress().done);
    let started = Instant::now();
    job.cancel();
    assert!(matches!(
        job.wait().unwrap(),
        CompressedOpenOutcome::Cancelled
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(entries(temp.path()).is_empty());

    let job =
        CompressedRawDecompressJob::start(&path, slow, WorkingCopyCleanup::default()).unwrap();
    assert!(pause.wait_for(2), "no raw members were decompressed");
    assert!(!job.progress().done);
    let started = Instant::now();
    job.cancel();
    assert!(job.wait().unwrap().is_none());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(entries(temp.path()).is_empty());
}

#[test]
fn a_stream_that_ends_exactly_at_the_bootstrap_limit_opens() {
    let dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let options = OpenOptions {
        rows: 100,
        sample_bytes: 8,
        bootstrap_limit: 8,
        ..OpenOptions::default()
    };
    let exact = write(dir.path(), "exact.csv.gz", &gzip(b"a,b\n1,2\n"));
    let bootstrap = CompressedBootstrap::open(&exact, options, None, None).unwrap();
    assert!(bootstrap.reached_eof());
    assert_eq!(bootstrap.decompressed_size(), Some(8));
    assert_eq!(bootstrap.dialect.delimiter, b',');
    assert_eq!(bootstrap.first_rows.len(), 2);
    assert_eq!(
        bootstrap.first_rows[1].fields,
        [b"1".to_vec(), b"2".to_vec()]
    );
    assert!(bootstrap.metrics.bootstrap_bytes_read <= 8);
    match bootstrap
        .start_decompression(config(temp.path()), WorkingCopyCleanup::default())
        .unwrap()
        .wait()
        .unwrap()
    {
        CompressedOpenOutcome::Complete { session, index } => {
            assert_eq!(session.file_size, 8);
            assert_eq!(index.indexed_rows(), 2);
        }
        CompressedOpenOutcome::Cancelled => panic!("unexpected cancellation"),
    }
    assert!(entries(temp.path()).is_empty());

    // Delimiter detection also needs the end: this sample holds no complete record.
    let unterminated = write(dir.path(), "semicolon.csv.gz", &gzip(b"a;b;c;dd"));
    let bootstrap = CompressedBootstrap::open(&unterminated, options, None, None).unwrap();
    assert!(bootstrap.reached_eof());
    assert_eq!(bootstrap.dialect.delimiter, b';');
    assert_eq!(bootstrap.first_rows.len(), 1);
    assert_eq!(bootstrap.first_rows[0].fields.len(), 4);

    // One more byte is genuinely past the limit.
    for data in [&b"a,b\n1,2\n3"[..], b"a;b;c;ddd"] {
        let longer = write(dir.path(), "longer.csv.gz", &gzip(data));
        assert!(
            matches!(
                CompressedBootstrap::open(&longer, options, None, None),
                Err(QuarryError::BootstrapLimitExceeded { limit: 8, .. })
            ),
            "{data:?}"
        );
    }
}

#[test]
fn every_writer_rejects_compressed_destination_names() {
    let dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    for name in ["out.csv.gz", "OUT.TGZ", "out.tgz", "Out.Csv.Gz"] {
        assert!(is_compressed_destination(Path::new(name)), "{name}");
        assert!(
            matches!(
                reject_compressed_destination(Path::new(name)),
                Err(QuarryError::InvalidOption(message))
                    if message.contains("without .gz or .tgz")
            ),
            "{name}"
        );
    }
    for name in ["out.csv", "out.gzip", "gz", "out.gz.csv", "out.tgz.tsv"] {
        assert!(!is_compressed_destination(Path::new(name)), "{name}");
        assert!(
            reject_compressed_destination(Path::new(name)).is_ok(),
            "{name}"
        );
    }

    let first = write(dir.path(), "first.csv", b"id,a\n1,x\n2,y\n");
    let second = write(dir.path(), "second.csv", b"id,a\n3,z\n");
    let other = write(dir.path(), "other.csv", b"id,b\n1,p\n2,q\n");
    let input = |path: &Path| JoinInput {
        path: path.to_path_buf(),
        delimiter: None,
        has_header: true,
        encoding: None,
        key_columns: vec![0],
        expected_version: None,
        require_unique: false,
    };
    let invalid = |error: &QuarryError| matches!(error, QuarryError::InvalidOption(message) if message.contains(".tgz"));
    let (session, _) = plain(&first, OpenOptions::default(), None);
    let report = InspectionJob::start(
        &first,
        InspectionOptions::default(),
        InspectionEdits::default(),
    )
    .unwrap()
    .wait()
    .unwrap();
    for name in ["combined.csv.gz", "combined.TGZ"] {
        let destination = dir.path().join(name);

        let plan = check_combine_files(
            vec![first.clone(), second.clone()],
            CombineOptions::default(),
        )
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
        let error = expect_err(plan.start_write(destination.clone()));
        assert!(invalid(&error), "combine {name}: {error:?}");

        let plan = analyze_join(
            [input(&first), input(&other)],
            temp.path().to_path_buf(),
            JoinConfig::default(),
        )
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
        let error = expect_err(plan.start_write(destination.clone()));
        assert!(invalid(&error), "join {name}: {error:?}");
        assert!(
            entries(temp.path()).is_empty(),
            "join {name} left its workspace"
        );

        let spec = crate::SortSpec {
            column: 0,
            mode: crate::SortMode::Text,
            direction: crate::SortDirection::Ascending,
            case_sensitivity: CaseSensitivity::Sensitive,
        };
        let error = expect_err(session.start_create_sorted_working_copy(
            BTreeMap::new(),
            BTreeMap::new(),
            spec,
            &destination,
        ));
        assert!(invalid(&error), "sort {name}: {error:?}");
        let error = expect_err(session.start_find_duplicates(
            BTreeMap::new(),
            BTreeMap::new(),
            crate::DuplicateSpec {
                columns: vec![0],
                case_sensitivity: CaseSensitivity::Sensitive,
            },
            &destination,
        ));
        assert!(invalid(&error), "duplicates {name}: {error:?}");
        let error = expect_err(
            session
                .start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), &destination)
                .and_then(|job| job.wait()),
        );
        assert!(invalid(&error), "save as {name}: {error:?}");
        let error = expect_err(RecoveryJob::start(&report, &BTreeSet::new(), &destination));
        assert!(invalid(&error), "recovery {name}: {error:?}");

        assert!(!destination.exists(), "{name} was written");
    }
}

#[test]
fn raw_decompression_keeps_malformed_text_for_inspection() {
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let data = b"id,name\n1,good\n2,\"bad\"x\n";
    let path = write(source_dir.path(), "bad.csv.gz", &gzip(data));
    // Opening parses the text, so malformed data fails before anything can be inspected.
    assert!(CompressedBootstrap::open(&path, OpenOptions::default(), None, None).is_err());

    let cleanup = WorkingCopyCleanup::default();
    let job =
        CompressedRawDecompressJob::start(&path, config(temp.path()), cleanup.clone()).unwrap();
    let copy = job.wait().unwrap().expect("not cancelled");
    assert_eq!(fs::read(copy.path()).unwrap(), data);
    assert!(copy.path().starts_with(temp.path()));
    assert_eq!(copy.decompressed_size(), data.len() as u64);
    assert_eq!(copy.source_path(), path);
    assert_eq!(copy.format(), CompressionFormat::Gzip);
    assert_eq!(copy.compressed_size(), fs::metadata(&path).unwrap().len());
    copy.ensure_source_unchanged().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(copy.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "private copy mode {mode:o}");
    }
    let report = InspectionJob::start(
        copy.path(),
        InspectionOptions::default(),
        InspectionEdits::default(),
    )
    .unwrap()
    .wait()
    .unwrap();
    // The same diagnosis as for the uncompressed text: a structural error stops
    // inspection, so recovery is refused exactly as it is for a plain file.
    assert_eq!(report.status, InspectionStatus::Stopped);
    assert_eq!(report.total_findings, 1);
    let finding = &report.findings[0];
    assert_eq!(finding.record, Some(3));
    assert_eq!(finding.column, Some(2));
    assert_eq!(finding.physical_line, Some(3));
    assert_eq!(finding.byte_offset, Some(22));
    let recovered = source_dir.path().join("recovered.csv");
    assert!(matches!(
        RecoveryJob::start(&report, &BTreeSet::from([0]), &recovered),
        Err(QuarryError::InvalidOption(_))
    ));
    assert!(!recovered.exists());
    drop(report);
    drop(copy);
    assert!(entries(temp.path()).is_empty());

    // A recoverable finding: omit the record from an uncompressed recovery copy.
    let encoded = write(
        source_dir.path(),
        "encoding.csv.gz",
        &gzip(b"id,name\n1,good\n2,b\xffd\n3,ok\n"),
    );
    let copy = CompressedRawDecompressJob::start(&encoded, config(temp.path()), cleanup.clone())
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
    let report = InspectionJob::start(
        copy.path(),
        InspectionOptions::default(),
        InspectionEdits::default(),
    )
    .unwrap()
    .wait()
    .unwrap();
    assert_eq!(report.status, InspectionStatus::Complete);
    assert_eq!(report.total_findings, 1);
    assert_eq!(report.findings[0].byte_offset, Some(18));
    let summary = RecoveryJob::start(&report, &BTreeSet::from([0]), &recovered)
        .unwrap()
        .wait()
        .unwrap()
        .unwrap();
    assert_eq!(summary.removed_records, 1);
    assert_eq!(fs::read(&recovered).unwrap(), b"id,name\n1,good\n3,ok\n");
    drop(report);
    drop(copy);
    assert!(entries(temp.path()).is_empty());
    assert!(cleanup.take_errors().is_empty());

    // Corruption, the size cap, the free-space reserve, a plain file and bad options
    // all fail before anything outlives the job.
    let valid = gzip(&numbered_rows(1_000));
    let mut bad_crc = valid.clone();
    let at = bad_crc.len() - 8;
    bad_crc[at] ^= 1;
    let bad = write(source_dir.path(), "crc.csv.gz", &bad_crc);
    let error = CompressedRawDecompressJob::start(&bad, config(temp.path()), cleanup.clone())
        .unwrap()
        .wait()
        .unwrap_err();
    assert!(reason(&error).contains("CRC-32"), "{error:?}");
    assert!(entries(temp.path()).is_empty());

    let rows = write(source_dir.path(), "rows.csv.gz", &valid);
    let job = CompressedRawDecompressJob::start(
        &rows,
        CompressedOpenConfig {
            max_decompressed_bytes: Some(100),
            ..config(temp.path())
        },
        cleanup.clone(),
    )
    .unwrap();
    wait_for(|| job.progress().done, "cap");
    assert!(job.error().unwrap().contains("limit"));
    assert!(matches!(
        job.wait(),
        Err(QuarryError::DecompressionLimitExceeded {
            limit_bytes: 100,
            ..
        })
    ));
    assert!(entries(temp.path()).is_empty());

    let error = expect_err(CompressedRawDecompressJob::start(
        &rows,
        CompressedOpenConfig {
            min_free_space_reserve_bytes: u64::MAX,
            space_check_interval_bytes: 1,
            ..config(temp.path())
        },
        cleanup.clone(),
    ));
    assert!(
        matches!(error, QuarryError::InsufficientStorage { .. }),
        "{error:?}"
    );
    assert!(entries(temp.path()).is_empty());

    let plain_file = write(source_dir.path(), "plain.csv", data);
    assert!(matches!(
        CompressedRawDecompressJob::start(&plain_file, config(temp.path()), cleanup.clone()),
        Err(QuarryError::CompressedFile { .. })
    ));
    assert!(matches!(
        CompressedRawDecompressJob::start(
            &rows,
            CompressedOpenConfig {
                max_decompressed_bytes: Some(0),
                ..config(temp.path())
            },
            cleanup.clone(),
        ),
        Err(QuarryError::InvalidOption(_))
    ));
    assert!(entries(temp.path()).is_empty());

    // A source replaced during decompression is caught by the final stamp check.
    let big = write(
        source_dir.path(),
        "big.csv.gz",
        &gzip(&numbered_rows(100_000)),
    );
    let pause = Pause::new(&big, Checkpoint::BeforeSourceCheck);
    let job = CompressedRawDecompressJob::start(
        &big,
        CompressedOpenConfig {
            index_config: IndexConfig {
                chunk_bytes: 8,
                ..IndexConfig::default()
            },
            ..config(temp.path())
        },
        cleanup.clone(),
    )
    .unwrap();
    // The raw copy is complete and held just before its final source check.
    assert!(pause.wait_for(1), "the raw worker never finished copying");
    fs::rename(&big, source_dir.path().join("moved.gz")).unwrap();
    fs::write(&big, gzip(b"different,source\n")).unwrap();
    pause.release();
    assert!(matches!(job.wait(), Err(QuarryError::SourceChanged)));
    assert!(entries(temp.path()).is_empty());
    assert!(cleanup.take_errors().is_empty());
}

#[test]
fn classifying_an_open_input_rewinds_it_and_reports_read_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "plain.csv", b"id,a\n1,x\n");
    let mut file = fs::File::open(&path).unwrap();
    file.seek(SeekFrom::Start(5)).unwrap();
    assert_eq!(
        compressed_file_problem(&mut file, &path, "Combine Files").unwrap(),
        None
    );
    assert_eq!(file.stream_position().unwrap(), 0);

    // A handle that cannot be read fails instead of passing as plain text.
    let mut unreadable = fs::OpenOptions::new().append(true).open(&path).unwrap();
    assert!(matches!(
        compression_of(&mut unreadable, &path),
        Err(QuarryError::Io(_))
    ));
    assert!(matches!(
        compressed_file_problem(&mut unreadable, &path, "Combine Files"),
        Err(QuarryError::Io(_))
    ));
    // A path-only check leaves I/O failures to the operation's own open.
    assert_eq!(
        compressed_input_problem(&dir.path().join("missing.csv"), "Combine Files"),
        None
    );
}

#[cfg(unix)]
#[test]
fn combine_judges_a_linked_input_by_the_name_that_was_chosen() {
    let dir = tempfile::tempdir().unwrap();
    let plain_file = write(dir.path(), "plain.csv", b"id,a\n1,x\n");
    let other = write(dir.path(), "other.csv", b"id,a\n2,y\n");
    let link = dir.path().join("link.csv.gz");
    std::os::unix::fs::symlink(&plain_file, &link).unwrap();
    assert!(detect_compression(&link).is_err());

    let error = expect_err(
        check_combine_files(vec![other, link.clone()], CombineOptions::default())
            .unwrap()
            .wait(),
    );
    assert!(
        matches!(&error, QuarryError::CombineFile { path, reason, .. }
            if path == &link && reason.contains("gzip signature")),
        "{error:?}"
    );
}

/// `data` followed by `empty` empty gzip members.
fn with_empty_members(data: &[u8], empty: usize) -> Vec<u8> {
    let mut bytes = if data.is_empty() {
        Vec::new()
    } else {
        gzip(data)
    };
    for _ in 0..empty {
        bytes.extend(gzip(b""));
    }
    bytes
}

#[test]
fn bootstrap_bounds_compressed_work_and_leaves_the_rest_to_the_background_pass() {
    const BUDGET: u64 = 4 * 1024;
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let data = b"a,b\n1,2\n";
    let bytes = with_empty_members(data, 2_000);
    assert!(bytes.len() as u64 > 4 * BUDGET);
    let path = write(source_dir.path(), "members.csv.gz", &bytes);

    // The budget runs out while reading rows, and again during the end-of-data
    // lookahead once every requested row is buffered.
    for rows in [OpenOptions::default().rows, 2] {
        let options = OpenOptions {
            rows,
            ..OpenOptions::default()
        };
        let bootstrap =
            CompressedBootstrap::open_with_budget(&path, options, None, None, BUDGET).unwrap();
        assert_eq!(bootstrap.decompressed_size(), None);
        assert!(bootstrap.compressed_bytes_read() <= BUDGET + data.len() as u64);
        assert_eq!(bootstrap.dialect.delimiter, b',');
        assert_eq!(
            bootstrap.first_rows[1].fields,
            [b"1".to_vec(), b"2".to_vec()]
        );
        assert_eq!(bootstrap.first_rows.len(), 2);
        let job = bootstrap
            .start_decompression(config(temp.path()), WorkingCopyCleanup::default())
            .unwrap();
        let CompressedOpenOutcome::Complete { session, index } = job.wait().unwrap() else {
            panic!("unexpected cancellation");
        };
        assert_eq!(session.file_size, data.len() as u64);
        assert_eq!(index.indexed_rows(), 2);
        drop(session);
        assert!(entries(temp.path()).is_empty());
    }

    // Without a complete row inside the budget, the open fails at once.
    for (name, data) in [("empty.csv.gz", &b""[..]), ("partial.csv.gz", b"a,b")] {
        let path = write(source_dir.path(), name, &with_empty_members(data, 2_000));
        let error = expect_err(CompressedBootstrap::open_with_budget(
            &path,
            OpenOptions::default(),
            None,
            None,
            BUDGET,
        ));
        assert!(reason(&error).contains("No complete row"), "{error:?}");
    }

    // A stream that ends within the budget is still verified to its exact end.
    let short = write(
        source_dir.path(),
        "short.csv.gz",
        &with_empty_members(data, 3),
    );
    let bootstrap =
        CompressedBootstrap::open_with_budget(&short, OpenOptions::default(), None, None, BUDGET)
            .unwrap();
    assert_eq!(bootstrap.decompressed_size(), Some(data.len() as u64));
    let bootstrap = CompressedBootstrap::open(&path, OpenOptions::default(), None, None).unwrap();
    assert_eq!(bootstrap.decompressed_size(), Some(data.len() as u64));
}

#[test]
fn free_space_is_checked_again_when_a_chunk_exceeds_the_remaining_headroom() {
    const MIB: u64 = 1024 * 1024;
    let source_dir = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let data = numbered_rows(300_000);
    assert!(data.len() as u64 > 3 * MIB);
    let path = write(source_dir.path(), "rows.csv.gz", &gzip(&data));
    let reserve = 4 * MIB;
    let job = CompressedRawDecompressJob::start(
        &path,
        CompressedOpenConfig {
            min_free_space_reserve_bytes: reserve,
            space_check_interval_bytes: 64 * MIB,
            available_space_override: Some(reserve + 3 * MIB / 2),
            index_config: IndexConfig {
                chunk_bytes: MIB as usize,
                ..IndexConfig::default()
            },
            ..config(temp.path())
        },
        WorkingCopyCleanup::default(),
    )
    .unwrap();
    wait_for(|| job.progress().done, "the free-space check");
    // The first chunk fits; the second is checked again and refused before it
    // could reach into the reserve.
    assert_eq!(job.progress().decompressed_bytes_written, MIB);
    let error = job.wait().unwrap_err();
    assert!(
        matches!(
            &error,
            QuarryError::InsufficientStorage { required_bytes, available_bytes, .. }
                if *required_bytes == reserve + MIB && *available_bytes == reserve + MIB / 2
        ),
        "{error:?}"
    );
    assert!(entries(temp.path()).is_empty());
}
