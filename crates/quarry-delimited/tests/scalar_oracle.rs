//! Independent pure-scalar reference tokenizer and parser oracle for `quarry-delimited`.
//!
//! This oracle intentionally avoids `memchr`, SIMD intrinsics, and fast-path skips.
//! It processes one byte at a time as an executable specification so future SIMD and
//! parallel scanners must agree byte-for-byte on record ends, widths, decoded fields,
//! and exact error kinds and offsets.

use std::borrow::Cow;

use quarry_delimited::{
    ByteEncodingClass, EscapeRule, ParseError, ParseErrorKind, RecordScanner, ScannerDialect,
    SimdBackend, StructuralScanner, active_simd_backend, parse_record, parse_record_with_dialect,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OracleState {
    FieldStart,
    Unquoted,
    Quoted,
    AfterQuote,
    AfterQuoteCr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OracleScanResult {
    ends: Vec<u64>,
    max_width: usize,
}

/// Naive byte-at-a-time reference scanner implementing the exact `StructuralScanner` contract.
fn scalar_oracle_scan(bytes: &[u8], delimiter: u8) -> Result<OracleScanResult, ParseError> {
    if matches!(delimiter, b'"' | b'\r' | b'\n') {
        return Err(ParseError {
            offset: 0,
            kind: ParseErrorKind::InvalidDelimiter,
        });
    }

    let (mut index, mut record_start) = if bytes.starts_with(b"\xEF\xBB\xBF") {
        (3usize, 3u64)
    } else {
        (0usize, 0u64)
    };

    let mut state = OracleState::FieldStart;
    let mut ends = Vec::new();
    let mut separators: usize = 0;
    let mut max_width: usize = 0;

    while index < bytes.len() {
        let byte = bytes[index];
        let offset = index as u64;
        match state {
            OracleState::FieldStart => {
                if byte == delimiter {
                    separators = separators.saturating_add(1);
                    state = OracleState::FieldStart;
                } else if byte == b'"' {
                    state = OracleState::Quoted;
                } else if byte == b'\n' {
                    max_width = max_width.max(separators.saturating_add(1));
                    separators = 0;
                    let end = offset + 1;
                    ends.push(end);
                    record_start = end;
                    state = OracleState::FieldStart;
                } else {
                    state = OracleState::Unquoted;
                }
            }
            OracleState::Unquoted => {
                if byte == delimiter {
                    separators = separators.saturating_add(1);
                    state = OracleState::FieldStart;
                } else if byte == b'"' {
                    return Err(ParseError {
                        offset,
                        kind: ParseErrorKind::UnexpectedQuote,
                    });
                } else if byte == b'\n' {
                    max_width = max_width.max(separators.saturating_add(1));
                    separators = 0;
                    let end = offset + 1;
                    ends.push(end);
                    record_start = end;
                    state = OracleState::FieldStart;
                }
            }
            OracleState::Quoted => {
                if byte == b'"' {
                    state = OracleState::AfterQuote;
                }
            }
            OracleState::AfterQuote => {
                if byte == b'"' {
                    state = OracleState::Quoted;
                } else if byte == delimiter {
                    separators = separators.saturating_add(1);
                    state = OracleState::FieldStart;
                } else if byte == b'\r' {
                    state = OracleState::AfterQuoteCr;
                } else if byte == b'\n' {
                    max_width = max_width.max(separators.saturating_add(1));
                    separators = 0;
                    let end = offset + 1;
                    ends.push(end);
                    record_start = end;
                    state = OracleState::FieldStart;
                } else {
                    return Err(ParseError {
                        offset,
                        kind: ParseErrorKind::UnexpectedAfterQuote(byte),
                    });
                }
            }
            OracleState::AfterQuoteCr => {
                if byte != b'\n' {
                    return Err(ParseError {
                        offset,
                        kind: ParseErrorKind::ExpectedLfAfterCr,
                    });
                }
                max_width = max_width.max(separators.saturating_add(1));
                separators = 0;
                let end = offset + 1;
                ends.push(end);
                record_start = end;
                state = OracleState::FieldStart;
            }
        }
        index += 1;
    }

    let absolute_end = bytes.len() as u64;
    match state {
        OracleState::Quoted => {
            return Err(ParseError {
                offset: absolute_end,
                kind: ParseErrorKind::UnterminatedQuote,
            });
        }
        OracleState::AfterQuoteCr => {
            return Err(ParseError {
                offset: absolute_end,
                kind: ParseErrorKind::ExpectedLfAfterCr,
            });
        }
        OracleState::FieldStart | OracleState::Unquoted | OracleState::AfterQuote => {}
    }

    if absolute_end > record_start {
        max_width = max_width.max(separators.saturating_add(1));
        ends.push(absolute_end);
    }

    Ok(OracleScanResult { ends, max_width })
}

/// Naive byte-at-a-time reference parser for a single record.
fn scalar_oracle_parse_record(
    mut record: &[u8],
    delimiter: u8,
) -> Result<Vec<Vec<u8>>, ParseError> {
    if matches!(delimiter, b'"' | b'\r' | b'\n') {
        return Err(ParseError {
            offset: 0,
            kind: ParseErrorKind::InvalidDelimiter,
        });
    }
    if let Some(without_lf) = record.strip_suffix(b"\n") {
        record = without_lf.strip_suffix(b"\r").unwrap_or(without_lf);
    }

    let mut fields = Vec::new();
    let mut index = 0usize;
    loop {
        if index == record.len() {
            fields.push(Vec::new());
            break;
        }
        if record[index] == b'"' {
            index += 1;
            let mut field = Vec::new();
            loop {
                if index == record.len() {
                    return Err(ParseError {
                        offset: record.len() as u64,
                        kind: ParseErrorKind::UnterminatedQuote,
                    });
                }
                let byte = record[index];
                if byte == b'"' {
                    if index + 1 < record.len() && record[index + 1] == b'"' {
                        field.push(b'"');
                        index += 2;
                    } else {
                        index += 1;
                        break;
                    }
                } else {
                    field.push(byte);
                    index += 1;
                }
            }
            fields.push(field);
            if index == record.len() {
                break;
            }
            if record[index] != delimiter {
                return Err(ParseError {
                    offset: index as u64,
                    kind: ParseErrorKind::UnexpectedAfterQuote(record[index]),
                });
            }
            index += 1;
        } else {
            let mut field = Vec::new();
            while index < record.len() && record[index] != delimiter {
                if record[index] == b'"' {
                    return Err(ParseError {
                        offset: index as u64,
                        kind: ParseErrorKind::UnexpectedQuote,
                    });
                }
                field.push(record[index]);
                index += 1;
            }
            fields.push(field);
            if index == record.len() {
                break;
            }
            index += 1;
        }
    }
    Ok(fields)
}

fn scan_with_chunks(
    input: &[u8],
    delimiter: u8,
    chunk_boundaries: &[usize],
) -> Result<OracleScanResult, ParseError> {
    let mut scanner = RecordScanner::new(delimiter)?;
    let mut width_scanner = StructuralScanner::<true>::new(delimiter)?;
    let mut ends = Vec::new();
    let mut width_ends = Vec::new();
    let mut start = 0usize;

    for &end in chunk_boundaries {
        let slice = &input[start..end];
        let r1 = scanner.scan_chunk(slice, start as u64, |pos| ends.push(pos));
        let r2 = width_scanner.scan_chunk(slice, start as u64, |pos| width_ends.push(pos));
        assert_eq!(r1, r2, "RecordScanner and StructuralScanner<true> diverged");
        r1?;
        start = end;
    }
    let slice = &input[start..];
    let r1 = scanner.scan_chunk(slice, start as u64, |pos| ends.push(pos));
    let r2 = width_scanner.scan_chunk(slice, start as u64, |pos| width_ends.push(pos));
    assert_eq!(r1, r2, "tail scan diverged");
    r1?;

    let f1 = scanner.finish(input.len() as u64, |pos| ends.push(pos));
    let f2 = width_scanner.finish(input.len() as u64, |pos| width_ends.push(pos));
    assert_eq!(f1, f2, "finish diverged");
    f1?;

    assert_eq!(ends, width_ends);
    Ok(OracleScanResult {
        ends,
        max_width: width_scanner.maximum_width(),
    })
}

fn verify_record_parsing(input: &[u8], ends: &[u64], delimiter: u8) {
    let mut start = if input.starts_with(b"\xEF\xBB\xBF") {
        3usize
    } else {
        0usize
    };
    for &end_u64 in ends {
        let end = end_u64 as usize;
        let record = &input[start..end];
        let actual: Vec<Vec<u8>> = parse_record(record, delimiter)
            .expect("valid scanned record must parse")
            .into_iter()
            .map(Cow::into_owned)
            .collect();
        let expected = scalar_oracle_parse_record(record, delimiter)
            .expect("oracle must agree on scanned record");
        assert_eq!(actual, expected, "record [{start}..{end}] diverged");
        start = end;
    }
}

#[test]
fn scalar_oracle_matches_scanner_and_parser_at_every_single_split() {
    let cases: &[&[u8]] = &[
        b"",
        b"\n",
        b"\r\n",
        b"\xEF\xBB\xBF",
        b"\xEF\xBB\xBFa,b\n",
        b"\xEF\xBB\xBF\"quoted\r\nline\",\"esc \"\"quote\"\"\"\r\nx,y",
        b"\xEF\xBBa,b\n",
        b"\xEFa,b\n",
        b"a,b\n\xEF\xBB\xBF\"c\",d\n",
        b"\"\"\"\",\"\"\"\"\"\",\",\",\r\n\"tail\nval\"\r\n",
        b"\"ok\"\r\n\"next\",2\n",
        b"\"bad\"\rx,2\n",
        b"\"bad\"\r",
        b"\"unterminated,field\nstill,quoted",
        b"unquoted\"bad,2\n",
        b"\"closed\"extra,2\n",
        b"a,b,c\n1\n1,2,3,4,5\n,,\n",
        b"col1\t\"col2\t\"\"q\"\"\"\r\ncol3\tcol4",
    ];

    for &delimiter in b",\t;|" {
        for &input in cases {
            let expected = scalar_oracle_scan(input, delimiter);
            for split in 0..=input.len() {
                let actual = scan_with_chunks(input, delimiter, &[split]);
                assert_eq!(
                    actual, expected,
                    "delimiter={delimiter:?} split={split} input={input:?}"
                );
            }
            if let Ok(ok) = &expected {
                verify_record_parsing(input, &ok.ends, delimiter);
            }
        }
    }
}

#[test]
fn scalar_oracle_matches_every_two_split_boundary_on_tricky_inputs() {
    let tricky: &[&[u8]] = &[
        b"\xEF\xBB\xBF\"a\"\"b\"\r\n\"c\nd\",e",
        b"\"a\"\r\n\"b\"\rx",
        b"\"a\"\"b\",c\n\"unterminated",
        b"\xEF\xBB\"x\",y\r\n",
    ];

    for &input in tricky {
        let expected = scalar_oracle_scan(input, b',');
        for s1 in 0..=input.len() {
            for s2 in s1..=input.len() {
                let actual = scan_with_chunks(input, b',', &[s1, s2]);
                assert_eq!(
                    actual, expected,
                    "two-split ({s1}, {s2}) failed on {input:?}"
                );
            }
        }
        if let Ok(ok) = &expected {
            verify_record_parsing(input, &ok.ends, b',');
        }
    }
}

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn gen_range(&mut self, upper: usize) -> usize {
        (self.next() as usize) % upper
    }
}

#[test]
fn scalar_oracle_matches_seeded_random_streams_and_error_offsets() {
    let mut rng = XorShift64(0x51A4_2026_1121_0001);
    let alphabet: &[u8] = b"ab,\"\r\n\t;| \xEF\xBB\xBF";
    let delimiters = [b',', b'\t', b';', b'|'];

    for case_idx in 0..300 {
        let delimiter = delimiters[case_idx % delimiters.len()];
        let len = rng.gen_range(128);
        let mut input = Vec::with_capacity(len + 3);
        if rng.gen_range(4) == 0 {
            input.extend_from_slice(b"\xEF\xBB\xBF");
        }
        if rng.gen_range(2) == 0 {
            // Structured valid CSV with occasional intentional faults.
            let rows = 1 + rng.gen_range(6);
            for r in 0..rows {
                let cols = 1 + rng.gen_range(5);
                for c in 0..cols {
                    if c > 0 {
                        input.push(delimiter);
                    }
                    if rng.gen_range(2) == 0 {
                        input.push(b'"');
                        let field_len = rng.gen_range(8);
                        for _ in 0..field_len {
                            let ch = alphabet[rng.gen_range(alphabet.len())];
                            if ch == b'"' {
                                input.extend_from_slice(b"\"\"");
                            } else {
                                input.push(ch);
                            }
                        }
                        input.push(b'"');
                    } else {
                        let field_len = rng.gen_range(6);
                        for _ in 0..field_len {
                            input.push(b'a' + (rng.gen_range(6) as u8));
                        }
                    }
                }
                if r + 1 < rows || rng.gen_range(2) == 0 {
                    if rng.gen_range(2) == 0 {
                        input.extend_from_slice(b"\r\n");
                    } else {
                        input.push(b'\n');
                    }
                }
            }
            // Inject a deliberate fault 25% of the time.
            if !input.is_empty() && rng.gen_range(4) == 0 {
                let pos = rng.gen_range(input.len());
                input[pos] = match rng.gen_range(3) {
                    0 => b'"',
                    1 => b'\r',
                    _ => b'X',
                };
            }
        } else {
            for _ in 0..len {
                input.push(alphabet[rng.gen_range(alphabet.len())]);
            }
        }

        let expected = scalar_oracle_scan(&input, delimiter);
        for chunk_size in [1usize, 2, 3, 5, 13, 64] {
            let mut boundaries = Vec::new();
            let mut pos = chunk_size;
            while pos < input.len() {
                boundaries.push(pos);
                pos += chunk_size;
            }
            let actual = scan_with_chunks(&input, delimiter, &boundaries);
            assert_eq!(
                actual, expected,
                "case={case_idx} delimiter={delimiter:?} chunk_size={chunk_size} input={input:?}"
            );
        }
        if let Ok(ok) = &expected {
            verify_record_parsing(&input, &ok.ends, delimiter);
        }
    }
}

#[test]
fn scanner_and_parser_handle_records_below_at_and_above_64_mib() {
    const LIMIT_64_MIB: usize = 64 * 1024 * 1024;
    let sizes = [LIMIT_64_MIB - 1, LIMIT_64_MIB, LIMIT_64_MIB + 64];

    for total_len in sizes {
        // Build: `"<padding>",z\n` with exact total length `total_len`.
        let overhead = 5; // opening `"`, closing `"`, `,`, `z`, `\n`
        let payload_len = total_len - overhead;
        let mut record = vec![b'a'; total_len];
        record[0] = b'"';
        record[1 + payload_len] = b'"';
        record[2 + payload_len] = b',';
        record[3 + payload_len] = b'z';
        record[4 + payload_len] = b'\n';

        // Scan in 1 MiB + odd-boundary chunks to verify multi-chunk framing across 64 MiB.
        let mut scanner = StructuralScanner::<true>::new(b',').unwrap();
        let mut ends = Vec::new();
        let chunk_size = 1024 * 1024 + 13;
        let mut offset = 0usize;
        while offset < record.len() {
            let next = (offset + chunk_size).min(record.len());
            scanner
                .scan_chunk(&record[offset..next], offset as u64, |end| ends.push(end))
                .unwrap();
            offset = next;
        }
        assert!(
            !scanner
                .finish(record.len() as u64, |end| ends.push(end))
                .unwrap()
        );
        assert_eq!(ends, vec![total_len as u64]);
        assert_eq!(scanner.maximum_width(), 2);

        let parsed = parse_record(&record, b',').unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].len(), payload_len);
        assert_eq!(parsed[1].as_ref(), b"z");
    }
}

#[test]
fn scalar_oracle_matches_parse_record_on_malformed_inputs_and_invalid_delimiters() {
    let malformed: &[&[u8]] = &[
        b"bad\"quote,2\n",
        b"a,bad\"quote\r\n",
        b"\"ok\"x,2\n",
        b"\"ok\" \n",
        b"\"unterminated,2",
        b"a,\"unterminated\n",
        b"\"a\"\"b\"c,d\n",
        b"\"ok\"\rx\n",
        b"\"\"\"x,2\n",
    ];

    for &delimiter in b",\t;|\"\r\n" {
        for &record in malformed {
            let actual = parse_record(record, delimiter)
                .map(|fields| fields.into_iter().map(Cow::into_owned).collect::<Vec<_>>());
            let expected = scalar_oracle_parse_record(record, delimiter);
            assert_eq!(
                actual, expected,
                "parse_record diverged for delimiter={delimiter:?} record={record:?}"
            );
        }
    }

    // Also compare random byte records (both valid and malformed) directly against the parser oracle.
    let mut rng = XorShift64(0x9A1F_0202_6112_1002);
    let alphabet: &[u8] = b"ab,\"\r\n\t;|x ";
    for case_idx in 0..250 {
        let delimiter = b",\t;|"[case_idx % 4];
        let len = rng.gen_range(48);
        let mut record = Vec::with_capacity(len);
        for _ in 0..len {
            record.push(alphabet[rng.gen_range(alphabet.len())]);
        }
        let actual = parse_record(&record, delimiter)
            .map(|fields| fields.into_iter().map(Cow::into_owned).collect::<Vec<_>>());
        let expected = scalar_oracle_parse_record(&record, delimiter);
        assert_eq!(
            actual, expected,
            "random parse_record case={case_idx} delimiter={delimiter:?} record={record:?}"
        );
    }
}

/// Independent pure-scalar reference scanner supporting arbitrary `ScannerDialect`.
fn scalar_oracle_scan_dialect(
    bytes: &[u8],
    dialect: ScannerDialect,
    encoding: ByteEncodingClass,
) -> Result<OracleScanResult, ParseError> {
    dialect.validate(encoding)?;

    let (mut index, mut record_start) =
        if encoding == ByteEncodingClass::Utf8 && bytes.starts_with(b"\xEF\xBB\xBF") {
            (3usize, 3u64)
        } else {
            (0usize, 0u64)
        };

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        FieldStart,
        Unquoted,
        Quoted,
        QuotedEscape,
        AfterQuote,
        AfterQuoteCr,
    }

    let mut state = State::FieldStart;
    let mut ends = Vec::new();
    let mut separators: usize = 0;
    let mut max_width: usize = 0;

    while index < bytes.len() {
        let byte = bytes[index];
        let offset = index as u64;
        match state {
            State::FieldStart => {
                if byte == dialect.delimiter {
                    separators = separators.saturating_add(1);
                } else if dialect.quote == Some(byte) {
                    state = State::Quoted;
                } else if byte == b'\n' {
                    max_width = max_width.max(separators.saturating_add(1));
                    separators = 0;
                    let end = offset + 1;
                    ends.push(end);
                    record_start = end;
                } else {
                    state = State::Unquoted;
                }
            }
            State::Unquoted => {
                if byte == dialect.delimiter {
                    separators = separators.saturating_add(1);
                    state = State::FieldStart;
                } else if dialect.quote == Some(byte) {
                    return Err(ParseError {
                        offset,
                        kind: ParseErrorKind::UnexpectedQuote,
                    });
                } else if byte == b'\n' {
                    max_width = max_width.max(separators.saturating_add(1));
                    separators = 0;
                    let end = offset + 1;
                    ends.push(end);
                    record_start = end;
                    state = State::FieldStart;
                }
            }
            State::Quoted => match dialect.escape {
                EscapeRule::DoubledQuote => {
                    if dialect.quote == Some(byte) {
                        state = State::AfterQuote;
                    }
                }
                EscapeRule::EscapeByte(esc) => {
                    if byte == esc {
                        state = State::QuotedEscape;
                    } else if dialect.quote == Some(byte) {
                        state = State::AfterQuote;
                    }
                }
            },
            State::QuotedEscape => {
                state = State::Quoted;
            }
            State::AfterQuote => {
                if dialect.escape == EscapeRule::DoubledQuote && dialect.quote == Some(byte) {
                    state = State::Quoted;
                } else if byte == dialect.delimiter {
                    separators = separators.saturating_add(1);
                    state = State::FieldStart;
                } else if byte == b'\r' {
                    state = State::AfterQuoteCr;
                } else if byte == b'\n' {
                    max_width = max_width.max(separators.saturating_add(1));
                    separators = 0;
                    let end = offset + 1;
                    ends.push(end);
                    record_start = end;
                    state = State::FieldStart;
                } else {
                    return Err(ParseError {
                        offset,
                        kind: ParseErrorKind::UnexpectedAfterQuote(byte),
                    });
                }
            }
            State::AfterQuoteCr => {
                if byte != b'\n' {
                    return Err(ParseError {
                        offset,
                        kind: ParseErrorKind::ExpectedLfAfterCr,
                    });
                }
                max_width = max_width.max(separators.saturating_add(1));
                separators = 0;
                let end = offset + 1;
                ends.push(end);
                record_start = end;
                state = State::FieldStart;
            }
        }
        index += 1;
    }

    let absolute_end = bytes.len() as u64;
    match state {
        State::Quoted | State::QuotedEscape => {
            return Err(ParseError {
                offset: absolute_end,
                kind: ParseErrorKind::UnterminatedQuote,
            });
        }
        State::AfterQuoteCr => {
            return Err(ParseError {
                offset: absolute_end,
                kind: ParseErrorKind::ExpectedLfAfterCr,
            });
        }
        State::FieldStart | State::Unquoted | State::AfterQuote => {}
    }

    if absolute_end > record_start {
        max_width = max_width.max(separators.saturating_add(1));
        ends.push(absolute_end);
    }

    Ok(OracleScanResult { ends, max_width })
}

fn run_dialect_backend(
    input: &[u8],
    dialect: ScannerDialect,
    encoding: ByteEncodingClass,
    backend: SimdBackend,
    chunk_size: usize,
) -> Result<OracleScanResult, ParseError> {
    let mut s_plain =
        StructuralScanner::<false>::with_dialect_and_backend(dialect, 0, encoding, backend)?;
    let mut s_width =
        StructuralScanner::<true>::with_dialect_and_backend(dialect, 0, encoding, backend)?;
    let mut ends_plain = Vec::new();
    let mut ends_width = Vec::new();
    let mut offset = 0usize;
    while offset < input.len() {
        let next = (offset + chunk_size).min(input.len());
        let r1 = s_plain.scan_chunk(&input[offset..next], offset as u64, |e| ends_plain.push(e));
        let r2 = s_width.scan_chunk(&input[offset..next], offset as u64, |e| ends_width.push(e));
        assert_eq!(r1, r2, "plain vs width diverged for backend={backend}");
        r1?;
        offset = next;
    }
    let f1 = s_plain.finish(input.len() as u64, |e| ends_plain.push(e));
    let f2 = s_width.finish(input.len() as u64, |e| ends_width.push(e));
    assert_eq!(f1, f2);
    f1?;
    assert_eq!(ends_plain, ends_width);
    Ok(OracleScanResult {
        ends: ends_width,
        max_width: s_width.maximum_width(),
    })
}

#[test]
fn simd_block_classifier_matches_oracle_across_every_64_byte_alignment() {
    let active = active_simd_backend();
    let dialects = [
        ScannerDialect::rfc4180(b','),
        ScannerDialect::new(b';', Some(b'\''), EscapeRule::DoubledQuote),
        ScannerDialect::new(b',', Some(b'"'), EscapeRule::EscapeByte(b'\\')),
        ScannerDialect::new(b'\t', None, EscapeRule::DoubledQuote),
    ];

    let payloads: &[&[u8]] = &[
        b"alpha,beta,gamma\n1,2,3\r\n4,5,6\n",
        b"\"quoted,field\",\"esc \"\"quote\"\"\",\"multi\nline\"\r\nplain,end\n",
        b"\"bs \\\"quote\\\"\",\"double \\\\\",\"trailing \\\\\\\"\"\r\nC:\\raw\\path,ok\n",
        b"\"bad\"after,2\n",
        b"unquoted\"bad,2\n",
        b"\"ok\"\rx,2\n",
        b"\"unterminated\nstill,open",
        b"'sq''esc';'multi\r\nline'\r\nnext;row\n",
        b"col1\t\"raw_quote\"\tcol3\ncol4\tcol5\n",
    ];

    for dialect in dialects {
        for &payload in payloads {
            // Slide payload across all 65 byte prefix alignments so every byte crosses a 64-byte boundary.
            for pad in 0..=64 {
                let mut input = vec![b'p'; pad];
                input.push(b'\n');
                input.extend_from_slice(payload);
                // Ensure input spans multiple 64-byte blocks.
                input.extend_from_slice(&[b'z'; 70]);
                if !payload.contains(&b'"') && !payload.contains(&b'\'') {
                    input.push(b'\n');
                }

                let expected = scalar_oracle_scan_dialect(&input, dialect, ByteEncodingClass::Utf8);
                for chunk_size in [64usize, 128, 131, input.len().max(1)] {
                    let scalar_res = run_dialect_backend(
                        &input,
                        dialect,
                        ByteEncodingClass::Utf8,
                        SimdBackend::Scalar,
                        chunk_size,
                    );
                    let simd_res = run_dialect_backend(
                        &input,
                        dialect,
                        ByteEncodingClass::Utf8,
                        active,
                        chunk_size,
                    );
                    assert_eq!(
                        scalar_res, expected,
                        "scalar mismatch dialect={dialect:?} pad={pad} chunk={chunk_size}"
                    );
                    assert_eq!(
                        simd_res, expected,
                        "simd ({active}) mismatch dialect={dialect:?} pad={pad} chunk={chunk_size}"
                    );
                }
            }
        }
    }
}

#[test]
fn simd_preserves_field_start_after_quote_across_quote_free_block() {
    let active = active_simd_backend();
    let dialect = ScannerDialect::rfc4180(b',');

    // Block 0 (64 bytes): `"aaaa...a"` -> ends in `AfterQuote`.
    // Block 1 (64 bytes, zero quotes): `\n` + 62 `a` + `,` -> transitions to `FieldStart`.
    // Block 2: `"b"\n` -> valid quoted field at offset 128.
    let mut after_quote_lf_then_delim = Vec::new();
    after_quote_lf_then_delim.push(b'"');
    after_quote_lf_then_delim.extend_from_slice(&[b'a'; 62]);
    after_quote_lf_then_delim.push(b'"');
    after_quote_lf_then_delim.push(b'\n');
    after_quote_lf_then_delim.extend_from_slice(&[b'a'; 62]);
    after_quote_lf_then_delim.push(b',');
    after_quote_lf_then_delim.extend_from_slice(b"\"b\"\n");

    // Same pattern with `AfterQuoteCr` at block 0/1 boundary (`"\r` at bytes 62..64).
    let mut after_quote_cr_lf_then_delim = Vec::new();
    after_quote_cr_lf_then_delim.push(b'"');
    after_quote_cr_lf_then_delim.extend_from_slice(&[b'a'; 61]);
    after_quote_cr_lf_then_delim.extend_from_slice(b"\"\r");
    after_quote_cr_lf_then_delim.push(b'\n');
    after_quote_cr_lf_then_delim.extend_from_slice(&[b'a'; 62]);
    after_quote_cr_lf_then_delim.push(b',');
    after_quote_cr_lf_then_delim.extend_from_slice(b"\"b\"\n");

    // Same pattern with `AfterQuote` followed immediately by `,` at byte 64 and `,` at byte 127.
    let mut after_quote_delim_then_delim = Vec::new();
    after_quote_delim_then_delim.push(b'"');
    after_quote_delim_then_delim.extend_from_slice(&[b'a'; 62]);
    after_quote_delim_then_delim.push(b'"');
    after_quote_delim_then_delim.push(b',');
    after_quote_delim_then_delim.extend_from_slice(&[b'a'; 62]);
    after_quote_delim_then_delim.push(b',');
    after_quote_delim_then_delim.extend_from_slice(b"\"b\"\n");

    for case in [
        after_quote_lf_then_delim,
        after_quote_cr_lf_then_delim,
        after_quote_delim_then_delim,
    ] {
        let expected = scalar_oracle_scan_dialect(&case, dialect, ByteEncodingClass::Utf8);
        assert!(expected.is_ok());
        for chunk_size in [64usize, 128, case.len()] {
            let scalar = run_dialect_backend(
                &case,
                dialect,
                ByteEncodingClass::Utf8,
                SimdBackend::Scalar,
                chunk_size,
            );
            let simd =
                run_dialect_backend(&case, dialect, ByteEncodingClass::Utf8, active, chunk_size);
            assert_eq!(scalar, expected);
            assert_eq!(simd, expected);
        }
    }
}

#[test]
fn simd_matches_scalar_on_large_seeded_quoted_and_escape_streams() {
    let active = active_simd_backend();
    let mut rng = XorShift64(0x2026_1121_51AD_0007);

    for dialect in [
        ScannerDialect::rfc4180(b','),
        ScannerDialect::new(b',', Some(b'"'), EscapeRule::EscapeByte(b'\\')),
        ScannerDialect::new(b'|', None, EscapeRule::DoubledQuote),
    ] {
        let mut input = Vec::with_capacity(64 * 1024);
        for row in 0..1000 {
            let cols = 1 + rng.gen_range(8);
            for col in 0..cols {
                if col > 0 {
                    input.push(dialect.delimiter);
                }
                if let Some(q) = dialect.quote
                    && rng.gen_range(3) == 0
                {
                    input.push(q);
                    let flen = rng.gen_range(24);
                    for _ in 0..flen {
                        match rng.gen_range(12) {
                            0 => match dialect.escape {
                                EscapeRule::DoubledQuote => {
                                    input.push(q);
                                    input.push(q);
                                }
                                EscapeRule::EscapeByte(esc) => {
                                    input.push(esc);
                                    input.push(q);
                                }
                            },
                            1 => match dialect.escape {
                                EscapeRule::DoubledQuote => input.push(b'\\'),
                                EscapeRule::EscapeByte(esc) => {
                                    input.push(esc);
                                    input.push(esc);
                                }
                            },
                            2 => input.push(b'\n'),
                            3 => input.push(dialect.delimiter),
                            _ => input.push(b'a' + (rng.gen_range(26) as u8)),
                        }
                    }
                    input.push(q);
                } else {
                    let flen = rng.gen_range(16);
                    for _ in 0..flen {
                        input.push(b'0' + (rng.gen_range(10) as u8));
                    }
                }
            }
            if row % 3 == 0 {
                input.extend_from_slice(b"\r\n");
            } else {
                input.push(b'\n');
            }
        }

        let expected =
            scalar_oracle_scan_dialect(&input, dialect, ByteEncodingClass::Utf8).unwrap();
        let simd_res =
            run_dialect_backend(&input, dialect, ByteEncodingClass::Utf8, active, 1024).unwrap();
        assert_eq!(
            simd_res, expected,
            "large seeded stream diverged for {dialect:?}"
        );

        // Verify parse_record_with_dialect succeeds on every scanned record.
        let mut start = 0usize;
        for &end in &expected.ends {
            let rec = &input[start..end as usize];
            let parsed = parse_record_with_dialect(rec, dialect, ByteEncodingClass::Utf8).unwrap();
            assert!(!parsed.is_empty());
            start = end as usize;
        }
    }
}

#[test]
#[ignore = "manual release-mode throughput measurement"]
fn measure_single_thread_simd_vs_scalar_throughput() {
    use std::time::Instant;

    let active = active_simd_backend();
    let unquoted_row = b"C0X100,2023-07-15,28.4,31.2,25.1,1012.3,78,2.4,180,0.0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20\n";
    let quoted_row = b"12345678,story,\"ClickHouse, \"\"quoted\"\" text with\nnewline\",author_name,2023-07-15 12:34:56,142,88,https://example.com/path\n";

    for (label, row) in [
        ("unquoted-weather-like", unquoted_row.as_slice()),
        ("quoted-hn-like", quoted_row.as_slice()),
    ] {
        let target_bytes = 64 * 1024 * 1024; // 64 MiB in-memory buffer
        let repeats = target_bytes / row.len();
        let mut buf = Vec::with_capacity(repeats * row.len());
        for _ in 0..repeats {
            buf.extend_from_slice(row);
        }
        let dialect = ScannerDialect::rfc4180(b',');

        // Warmup + measure scalar (TRACK_WIDTH = true, matching structural indexer).
        let mut scalar_records = 0u64;
        let t0 = Instant::now();
        for _ in 0..5 {
            let mut s = StructuralScanner::<true>::with_dialect_and_backend(
                dialect,
                0,
                ByteEncodingClass::Utf8,
                SimdBackend::Scalar,
            )
            .unwrap();
            scalar_records = s.scan_chunk(&buf, 0, |_| {}).unwrap();
            s.finish(buf.len() as u64, |_| {}).unwrap();
        }
        let scalar_secs = t0.elapsed().as_secs_f64() / 5.0;

        // Warmup + measure active SIMD backend.
        let mut simd_records = 0u64;
        let t1 = Instant::now();
        for _ in 0..5 {
            let mut s = StructuralScanner::<true>::with_dialect_and_backend(
                dialect,
                0,
                ByteEncodingClass::Utf8,
                active,
            )
            .unwrap();
            simd_records = s.scan_chunk(&buf, 0, |_| {}).unwrap();
            s.finish(buf.len() as u64, |_| {}).unwrap();
        }
        let simd_secs = t1.elapsed().as_secs_f64() / 5.0;

        assert_eq!(scalar_records, simd_records);
        let gb = buf.len() as f64 / 1e9;
        println!(
            "{label}: scalar={:.2} GB/s ({:.3} ms), {active}={:.2} GB/s ({:.3} ms), speedup={:.2}x",
            gb / scalar_secs,
            scalar_secs * 1000.0,
            gb / simd_secs,
            simd_secs * 1000.0,
            scalar_secs / simd_secs
        );
    }
}
