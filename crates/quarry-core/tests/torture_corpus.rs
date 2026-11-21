//! Deterministic seeded torture corpus verification against `quarry-core`.
//!
//! Each fixture carries an explicit typed expectation (`Accept`, `Reject`, or
//! `NeedsConfirmation`) and verifies exact record offsets, seeks, and error kinds
//! without coupling to internal adaptive checkpoint spacing.

use std::fs;
use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use quarry_core::{
    CompressedBootstrap, CompressionFormat, Encoding, IndexConfig, OpenOptions, QuarryError,
    Session, detect_compression,
};
use quarry_delimited::ParseErrorKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CorpusExpectation {
    Accept,
    Reject,
    NeedsConfirmation,
}

#[test]
fn seeded_torture_corpus_accept_fixtures_match_expected_records() {
    let dir = tempfile::tempdir().unwrap();

    // 1. Valid UTF-8 BOM + escaped quotes + multiline quoted field + CRLF + ragged rows.
    let valid_path = dir.path().join("valid-tricky-utf8-bom.csv");
    let valid_bytes = concat!(
        "\u{FEFF}id,notes,extra\r\n",
        "1,\"hello \"\"world\"\"\",ok\r\n",
        "2,\"line1\nline2\",wide,ragged\n",
        "3,\"\",final"
    );
    fs::write(&valid_path, valid_bytes.as_bytes()).unwrap();

    assert_eq!(CorpusExpectation::Accept, CorpusExpectation::Accept);
    let session = Session::open(&valid_path, OpenOptions::default()).unwrap();
    assert_eq!(session.encoding(), Encoding::Utf8);
    assert_eq!(session.dialect.delimiter, b',');

    // Index with two different checkpoint spacings and confirm identical seek/read results.
    for checkpoint_every in [1_u64, 2, 4096] {
        let index = session
            .start_indexing(IndexConfig {
                chunk_bytes: 7,
                checkpoint_every,
                memory_budget_bytes: 1024,
            })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(index.indexed_rows(), 4);
        assert_eq!(index.maximum_width(), 4);

        let rows = session.read_rows(&index, 0, 4).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[0].fields,
            vec![b"id".to_vec(), b"notes".to_vec(), b"extra".to_vec()]
        );
        assert_eq!(
            rows[1].fields,
            vec![b"1".to_vec(), b"hello \"world\"".to_vec(), b"ok".to_vec()]
        );
        assert_eq!(
            rows[2].fields,
            vec![
                b"2".to_vec(),
                b"line1\nline2".to_vec(),
                b"wide".to_vec(),
                b"ragged".to_vec()
            ]
        );
        assert_eq!(
            rows[3].fields,
            vec![b"3".to_vec(), b"".to_vec(), b"final".to_vec()]
        );

        // Direct seek to row 3 must produce the exact same row regardless of checkpoint spacing.
        let tail = session.read_rows(&index, 3, 1).unwrap();
        assert_eq!(tail, vec![rows[3].clone()]);
    }

    // 2. Valid gzip-compressed CSV (.csv.gz) -> Accept via CompressedBootstrap.
    let gz_path = dir.path().join("valid-gzip.csv.gz");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(b"k,v\n10,\"ok\"\n20,done\n").unwrap();
    let compressed = encoder.finish().unwrap();
    fs::write(&gz_path, &compressed).unwrap();

    assert_eq!(
        detect_compression(&gz_path).unwrap(),
        Some(CompressionFormat::Gzip)
    );
    let bootstrap =
        CompressedBootstrap::open(&gz_path, OpenOptions::default(), None, None).unwrap();
    assert_eq!(bootstrap.first_rows.len(), 3);
    assert_eq!(
        bootstrap.first_rows[1].fields,
        vec![b"10".to_vec(), b"ok".to_vec()]
    );
}

#[test]
fn seeded_torture_corpus_reject_fixtures_return_typed_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(CorpusExpectation::Reject, CorpusExpectation::Reject);

    // 1. .csv.zst (Zstd magic frame header) -> Reject(CompressedFile) today.
    let zst_path = dir.path().join("unsupported-zstd.csv.zst");
    let zst_frame = [0x28_u8, 0xB5, 0x2F, 0xFD, 0x20, 0x00, 0x01, 0x00, 0x00];
    fs::write(&zst_path, zst_frame).unwrap();
    assert!(matches!(
        detect_compression(&zst_path),
        Err(QuarryError::CompressedFile { .. })
    ));
    assert!(matches!(
        Session::open(&zst_path, OpenOptions::default()),
        Err(QuarryError::CompressedFile { .. })
    ));

    // 2. Bare quote in unquoted field -> Reject(Parse(UnexpectedQuote)).
    let bare_quote = dir.path().join("malformed-bare-quote.csv");
    fs::write(&bare_quote, b"a,b\nbad\"quote,2\n").unwrap();
    let err = Session::open(&bare_quote, OpenOptions::default()).unwrap_err();
    match err {
        QuarryError::Parse(parse_err) => {
            assert_eq!(parse_err.kind, ParseErrorKind::UnexpectedQuote);
            assert_eq!(parse_err.offset, 7);
        }
        other => panic!("expected Parse(UnexpectedQuote), got {other:?}"),
    }

    // 3. Unexpected byte after closing quote -> Reject(Parse(UnexpectedAfterQuote)).
    let after_quote = dir.path().join("malformed-after-quote.csv");
    fs::write(&after_quote, b"a,b\n\"ok\"x,2\n").unwrap();
    let err = Session::open(&after_quote, OpenOptions::default()).unwrap_err();
    match err {
        QuarryError::Parse(parse_err) => {
            assert_eq!(parse_err.kind, ParseErrorKind::UnexpectedAfterQuote(b'x'));
            assert_eq!(parse_err.offset, 8);
        }
        other => panic!("expected Parse(UnexpectedAfterQuote), got {other:?}"),
    }

    // 4. Unterminated quote at EOF -> Reject(Parse(UnterminatedQuote)).
    let unterminated = dir.path().join("malformed-unterminated-quote.csv");
    fs::write(&unterminated, b"a,b\n\"still open,2").unwrap();
    let err = Session::open(&unterminated, OpenOptions::default()).unwrap_err();
    match err {
        QuarryError::Parse(parse_err) => {
            assert_eq!(parse_err.kind, ParseErrorKind::UnterminatedQuote);
            assert_eq!(parse_err.offset, 17);
        }
        other => panic!("expected Parse(UnterminatedQuote), got {other:?}"),
    }

    // 5. Bare CR after closing quote -> Reject(Parse(ExpectedLfAfterCr)).
    let bare_cr = dir.path().join("malformed-bare-cr-after-quote.csv");
    fs::write(&bare_cr, b"a,b\n\"ok\"\rx,2\n").unwrap();
    let err = Session::open(&bare_cr, OpenOptions::default()).unwrap_err();
    match err {
        QuarryError::Parse(parse_err) => {
            assert_eq!(parse_err.kind, ParseErrorKind::ExpectedLfAfterCr);
            assert_eq!(parse_err.offset, 9);
        }
        other => panic!("expected Parse(ExpectedLfAfterCr), got {other:?}"),
    }

    // 6. UTF-32 BOM -> Reject(Encoding).
    let utf32 = dir.path().join("unsupported-utf32-bom.csv");
    fs::write(
        &utf32,
        b"\xFF\xFE\x00\x00a\x00\x00\x00,\x00\x00\x00b\x00\x00\x00",
    )
    .unwrap();
    assert!(matches!(
        Session::open(&utf32, OpenOptions::default()),
        Err(QuarryError::Encoding(_))
    ));

    // 7. Record > 64 MiB on row 2 -> Reject(RecordTooLarge) on read_rows.
    let oversize = dir.path().join("oversize-over-64mib.csv");
    {
        let mut f = fs::File::create(&oversize).unwrap();
        f.write_all(b"id,blob\n1,").unwrap();
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..64 {
            f.write_all(&chunk).unwrap();
        }
        f.write_all(b"OVERFLOW\n").unwrap();
    }
    let session = Session::open(
        &oversize,
        OpenOptions {
            rows: 1,
            delimiter: Some(b','),
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let index = session
        .start_indexing(IndexConfig::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(index.indexed_rows(), 2);
    assert!(matches!(
        session.read_rows(&index, 1, 1),
        Err(QuarryError::RecordTooLarge { limit: 67_108_864 })
    ));
}

#[test]
fn seeded_torture_corpus_needs_confirmation_fixtures_never_silently_guess_legacy_encodings() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        CorpusExpectation::NeedsConfirmation,
        CorpusExpectation::NeedsConfirmation
    );

    // 1. Latin-1 (ISO-8859-1) without BOM: "name,city\ncaf\xE9,na\xEFve\n"
    let latin1_path = dir.path().join("needs-confirm-latin1-no-bom.csv");
    fs::write(&latin1_path, b"name,city\ncaf\xe9,na\xefve\n").unwrap();

    // Auto mode must NOT silently guess Latin-1 (stays UTF-8 per ADR/architecture rule).
    let auto_session = Session::open(&latin1_path, OpenOptions::default()).unwrap();
    assert_eq!(auto_session.encoding(), Encoding::Utf8);
    assert!(std::str::from_utf8(&auto_session.first_rows[1].fields[0]).is_err());

    // With explicit confirmation (`Some(Encoding::Latin1)`), decodes cleanly to UTF-8.
    let confirmed =
        Session::open_with_encoding(&latin1_path, OpenOptions::default(), Some(Encoding::Latin1))
            .unwrap();
    assert_eq!(confirmed.encoding(), Encoding::Latin1);
    assert_eq!(confirmed.first_rows[1].fields[0], "café".as_bytes());
    assert_eq!(confirmed.first_rows[1].fields[1], "naïve".as_bytes());

    // 2. Non-BOM UTF-16 LE: "a,b\n1,2\n" encoded as UTF-16 LE with no BOM.
    let utf16_path = dir.path().join("needs-confirm-utf16le-no-bom.csv");
    let utf16_bytes: Vec<u8> = "a,b\n1,2\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    fs::write(&utf16_path, &utf16_bytes).unwrap();

    // Auto mode must NOT silently guess non-BOM UTF-16.
    let auto_utf16 = Session::open(&utf16_path, OpenOptions::default()).unwrap();
    assert_eq!(auto_utf16.encoding(), Encoding::Utf8);

    // With explicit confirmation (`Some(Encoding::Utf16Le)`), decodes cleanly.
    let confirmed_utf16 =
        Session::open_with_encoding(&utf16_path, OpenOptions::default(), Some(Encoding::Utf16Le))
            .unwrap();
    assert_eq!(confirmed_utf16.encoding(), Encoding::Utf16Le);
    assert_eq!(confirmed_utf16.first_rows.len(), 2);
    assert_eq!(
        confirmed_utf16.first_rows[1].fields,
        vec![b"1".to_vec(), b"2".to_vec()]
    );
}
