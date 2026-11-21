//! Independent pure-scalar reference tokenizer and parser oracle for `quarry-delimited`.
//!
//! This oracle intentionally avoids `memchr`, SIMD intrinsics, and fast-path skips.
//! It processes one byte at a time as an executable specification so future SIMD and
//! parallel scanners must agree byte-for-byte on record ends, widths, decoded fields,
//! and exact error kinds and offsets.

use std::borrow::Cow;

use quarry_delimited::{
    ParseError, ParseErrorKind, RecordScanner, StructuralScanner, parse_record,
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
