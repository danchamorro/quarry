//! Byte-oriented delimited record scanning and parsing.

use std::borrow::Cow;
use std::error::Error;
use std::fmt;

use memchr::{memchr, memchr3};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    InvalidDelimiter,
    UnexpectedQuote,
    UnexpectedAfterQuote(u8),
    UnterminatedQuote,
    ExpectedLfAfterCr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub offset: u64,
    pub kind: ParseErrorKind,
}

impl ParseError {
    fn new(offset: u64, kind: ParseErrorKind) -> Self {
        Self { offset, kind }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ParseErrorKind::InvalidDelimiter => write!(f, "invalid delimiter"),
            ParseErrorKind::UnexpectedQuote => {
                write!(f, "unexpected quote at byte {}", self.offset)
            }
            ParseErrorKind::UnexpectedAfterQuote(byte) => write!(
                f,
                "unexpected byte 0x{byte:02x} after closing quote at byte {}",
                self.offset
            ),
            ParseErrorKind::UnterminatedQuote => {
                write!(f, "unterminated quoted field at byte {}", self.offset)
            }
            ParseErrorKind::ExpectedLfAfterCr => {
                write!(f, "expected LF after CR at byte {}", self.offset)
            }
        }
    }
}

impl Error for ParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    FieldStart,
    Unquoted,
    Quoted,
    AfterQuote,
    AfterQuoteCr,
}

/// Stateful scanner shared by viewport reads and structural indexing.
#[derive(Debug, Clone)]
pub struct RecordScanner {
    delimiter: u8,
    state: ScanState,
    record_start: u64,
}

impl RecordScanner {
    pub fn new(delimiter: u8) -> Result<Self, ParseError> {
        Self::at_offset(delimiter, 0)
    }

    pub fn at_offset(delimiter: u8, record_start: u64) -> Result<Self, ParseError> {
        validate_delimiter(delimiter)?;
        Ok(Self {
            delimiter,
            state: ScanState::FieldStart,
            record_start,
        })
    }

    pub fn record_start(&self) -> u64 {
        self.record_start
    }

    /// Scans one contiguous chunk and emits exclusive absolute record ends.
    pub fn scan_chunk(
        &mut self,
        bytes: &[u8],
        absolute_start: u64,
        mut on_record_end: impl FnMut(u64),
    ) -> Result<u64, ParseError> {
        let mut index = 0;
        let mut records = 0;

        while index < bytes.len() {
            match self.state {
                ScanState::FieldStart | ScanState::Unquoted => {
                    let Some(relative) = memchr3(self.delimiter, b'"', b'\n', &bytes[index..])
                    else {
                        if self.state == ScanState::FieldStart {
                            self.state = ScanState::Unquoted;
                        }
                        break;
                    };
                    let found = index + relative;
                    if found > index && self.state == ScanState::FieldStart {
                        self.state = ScanState::Unquoted;
                        index = found;
                        continue;
                    }

                    match bytes[found] {
                        byte if byte == self.delimiter => {
                            self.state = ScanState::FieldStart;
                            index = found + 1;
                        }
                        b'"' if self.state == ScanState::FieldStart => {
                            self.state = ScanState::Quoted;
                            index = found + 1;
                        }
                        b'"' => {
                            return Err(ParseError::new(
                                absolute_start + found as u64,
                                ParseErrorKind::UnexpectedQuote,
                            ));
                        }
                        b'\n' => {
                            let end = absolute_start + found as u64 + 1;
                            on_record_end(end);
                            records += 1;
                            self.record_start = end;
                            self.state = ScanState::FieldStart;
                            index = found + 1;
                        }
                        _ => unreachable!(),
                    }
                }
                ScanState::Quoted => {
                    let Some(relative) = memchr(b'"', &bytes[index..]) else {
                        break;
                    };
                    index += relative + 1;
                    self.state = ScanState::AfterQuote;
                }
                ScanState::AfterQuote => match bytes[index] {
                    b'"' => {
                        self.state = ScanState::Quoted;
                        index += 1;
                    }
                    byte if byte == self.delimiter => {
                        self.state = ScanState::FieldStart;
                        index += 1;
                    }
                    b'\r' => {
                        self.state = ScanState::AfterQuoteCr;
                        index += 1;
                    }
                    b'\n' => {
                        let end = absolute_start + index as u64 + 1;
                        on_record_end(end);
                        records += 1;
                        self.record_start = end;
                        self.state = ScanState::FieldStart;
                        index += 1;
                    }
                    byte => {
                        return Err(ParseError::new(
                            absolute_start + index as u64,
                            ParseErrorKind::UnexpectedAfterQuote(byte),
                        ));
                    }
                },
                ScanState::AfterQuoteCr => {
                    if bytes[index] != b'\n' {
                        return Err(ParseError::new(
                            absolute_start + index as u64,
                            ParseErrorKind::ExpectedLfAfterCr,
                        ));
                    }
                    let end = absolute_start + index as u64 + 1;
                    on_record_end(end);
                    records += 1;
                    self.record_start = end;
                    self.state = ScanState::FieldStart;
                    index += 1;
                }
            }
        }

        Ok(records)
    }

    /// Finishes an EOF-delimited final record and validates quote state.
    pub fn finish(
        &mut self,
        absolute_end: u64,
        mut on_record_end: impl FnMut(u64),
    ) -> Result<bool, ParseError> {
        match self.state {
            ScanState::Quoted => {
                return Err(ParseError::new(
                    absolute_end,
                    ParseErrorKind::UnterminatedQuote,
                ));
            }
            ScanState::AfterQuoteCr => {
                return Err(ParseError::new(
                    absolute_end,
                    ParseErrorKind::ExpectedLfAfterCr,
                ));
            }
            _ => {}
        }

        if absolute_end > self.record_start {
            on_record_end(absolute_end);
            self.record_start = absolute_end;
            self.state = ScanState::FieldStart;
            return Ok(true);
        }
        Ok(false)
    }
}

pub fn parse_record(record: &[u8], delimiter: u8) -> Result<Vec<Cow<'_, [u8]>>, ParseError> {
    validate_delimiter(delimiter)?;
    let record = strip_record_ending(record);
    let mut fields: Vec<Cow<'_, [u8]>> = Vec::new();
    let mut start = 0;

    loop {
        if start == record.len() {
            fields.push(Cow::Borrowed(&[]));
            break;
        }

        if record[start] == b'"' {
            let content_start = start + 1;
            let mut cursor = content_start;
            let mut segment_start = content_start;
            let mut unescaped: Option<Vec<u8>> = None;

            loop {
                let Some(relative) = memchr(b'"', &record[cursor..]) else {
                    return Err(ParseError::new(
                        record.len() as u64,
                        ParseErrorKind::UnterminatedQuote,
                    ));
                };
                let quote = cursor + relative;

                if record.get(quote + 1) == Some(&b'"') {
                    let output = unescaped.get_or_insert_with(|| {
                        Vec::with_capacity(record.len().saturating_sub(content_start))
                    });
                    output.extend_from_slice(&record[segment_start..quote]);
                    output.push(b'"');
                    cursor = quote + 2;
                    segment_start = cursor;
                    continue;
                }

                let field = if let Some(mut output) = unescaped {
                    output.extend_from_slice(&record[segment_start..quote]);
                    Cow::Owned(output)
                } else {
                    Cow::Borrowed(&record[content_start..quote])
                };
                fields.push(field);

                let after_quote = quote + 1;
                if after_quote == record.len() {
                    return Ok(fields);
                }
                if record[after_quote] != delimiter {
                    return Err(ParseError::new(
                        after_quote as u64,
                        ParseErrorKind::UnexpectedAfterQuote(record[after_quote]),
                    ));
                }
                start = after_quote + 1;
                break;
            }
        } else {
            let end = memchr(delimiter, &record[start..])
                .map(|relative| start + relative)
                .unwrap_or(record.len());
            if let Some(relative) = memchr(b'"', &record[start..end]) {
                return Err(ParseError::new(
                    (start + relative) as u64,
                    ParseErrorKind::UnexpectedQuote,
                ));
            }
            fields.push(Cow::Borrowed(&record[start..end]));
            if end == record.len() {
                return Ok(fields);
            }
            start = end + 1;
        }
    }

    Ok(fields)
}

fn strip_record_ending(mut record: &[u8]) -> &[u8] {
    if record.ends_with(b"\n") {
        record = &record[..record.len() - 1];
        if record.ends_with(b"\r") {
            record = &record[..record.len() - 1];
        }
    }
    record
}

fn validate_delimiter(delimiter: u8) -> Result<(), ParseError> {
    if matches!(delimiter, b'"' | b'\r' | b'\n') {
        return Err(ParseError::new(0, ParseErrorKind::InvalidDelimiter));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{ParseErrorKind, RecordScanner, parse_record};

    fn fields(input: &[u8]) -> Vec<Vec<u8>> {
        parse_record(input, b',')
            .unwrap()
            .into_iter()
            .map(Cow::into_owned)
            .collect()
    }

    #[test]
    fn parses_required_csv_forms() {
        assert_eq!(fields(b"a,b,c\n"), [b"a", b"b", b"c"]);
        assert_eq!(fields(b"1,2,3\n"), [b"1", b"2", b"3"]);
        assert_eq!(
            fields(b"\"hello, world\",2,3\n"),
            [b"hello, world".as_slice(), b"2", b"3"]
        );
        assert_eq!(
            fields(b"\"a \"\"quoted\"\" value\",2,3\n"),
            [b"a \"quoted\" value".as_slice(), b"2", b"3"]
        );
        assert_eq!(
            fields(b"\"first\nsecond\",2,3\r\n"),
            [b"first\nsecond".as_slice(), b"2", b"3"]
        );
    }

    #[test]
    fn handles_empty_records_fields_and_trailing_delimiters() {
        assert_eq!(fields(b"\n"), [b"".as_slice()]);
        assert_eq!(fields(b",a,,\n"), [b"".as_slice(), b"a", b"", b""]);
        assert_eq!(fields(b"\"\",\"\"\n"), [b"".as_slice(), b""]);
    }

    #[test]
    fn scanner_is_correct_at_every_chunk_boundary() {
        let input = b"a,b\n\"quoted\nvalue\",\"escaped \"\"quote\"\"\"\r\nx,y";
        let expected = [4_u64, 40, input.len() as u64];

        for split in 0..=input.len() {
            let mut scanner = RecordScanner::new(b',').unwrap();
            let mut ends = Vec::new();
            scanner
                .scan_chunk(&input[..split], 0, |end| ends.push(end))
                .unwrap();
            scanner
                .scan_chunk(&input[split..], split as u64, |end| ends.push(end))
                .unwrap();
            scanner
                .finish(input.len() as u64, |end| ends.push(end))
                .unwrap();
            assert_eq!(ends, expected, "split {split}");
        }
    }

    #[test]
    fn handles_large_fields_without_per_byte_objects() {
        let field = vec![b'x'; 1_048_576];
        let mut record = field.clone();
        record.extend_from_slice(b",tail\n");
        let parsed = parse_record(&record, b',').unwrap();
        assert_eq!(parsed[0].as_ref(), field);
        assert_eq!(parsed[1].as_ref(), b"tail");
    }

    #[test]
    fn exposes_malformed_quote_sequences() {
        assert_eq!(
            parse_record(b"bad\"quote,2", b',').unwrap_err().kind,
            ParseErrorKind::UnexpectedQuote
        );
        assert!(matches!(
            parse_record(b"\"bad\"x,2", b',').unwrap_err().kind,
            ParseErrorKind::UnexpectedAfterQuote(b'x')
        ));

        let mut scanner = RecordScanner::new(b',').unwrap();
        scanner.scan_chunk(b"\"unfinished", 0, |_| {}).unwrap();
        assert_eq!(
            scanner.finish(11, |_| {}).unwrap_err().kind,
            ParseErrorKind::UnterminatedQuote
        );
    }
}
